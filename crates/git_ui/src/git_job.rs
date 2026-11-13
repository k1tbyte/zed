//! Local git jobs reported as toasts, and the prompts that retry an update or checkout past
//! uncommitted changes or a diverged upstream.

use anyhow::anyhow;
use git::repository::Branch;
use git_ops::{Blocker, LocalChanges, LocalRepo, Reconcile, UpdateOptions};
use git_ui_core::local_git::{local_repo, queue_git_job};
use git_ui_core::notifications::show_error_toast;
use gpui::{App, AppContext, Entity, PromptLevel, SharedString, WeakEntity, Window};
use project::git_store::Repository;
use std::future::Future;
use util::ResultExt as _;
use workspace::{Toast, Workspace, notifications::NotificationId};

pub(crate) struct GitOperation {
    pub(crate) job: &'static str,
    pub(crate) progress: String,
    pub(crate) success: String,
    pub(crate) command: String,
}

/// Progress labels of the jobs still running, for the branches sidebar's footer.
#[derive(Default)]
pub(crate) struct RunningJobs {
    labels: Vec<(usize, SharedString)>,
    next_id: usize,
}

impl RunningJobs {
    pub(crate) fn label(&self) -> Option<String> {
        let (_, first) = self.labels.first()?;
        Some(match self.labels.len() - 1 {
            0 => first.to_string(),
            more => format!("{first} (+{more} more)"),
        })
    }
}

/// Where a job is queued and reported, and optionally listed while it runs.
#[derive(Clone)]
pub(crate) struct JobContext {
    pub(crate) workspace: WeakEntity<Workspace>,
    pub(crate) repository: Entity<Repository>,
    pub(crate) running: Option<Entity<RunningJobs>>,
}

/// Queues `work` behind the repository's own jobs. `recover` may take over a failure, such as
/// to ask how to retry, instead of the error toast.
pub(crate) fn run_git_job(
    context: &JobContext,
    operation: GitOperation,
    work: impl Future<Output = anyhow::Result<()>> + Send + 'static,
    recover: impl FnOnce(&anyhow::Error, &mut Window, &mut App) -> bool + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let running = context.running.clone().map(|running| {
        let id = running.update(cx, |running, cx| {
            let id = running.next_id;
            running.next_id += 1;
            running.labels.push((id, operation.progress.clone().into()));
            cx.notify();
            id
        });
        (running, id)
    });
    let queued = queue_git_job(
        &context.repository,
        operation.job,
        operation.progress.clone(),
        work,
        cx,
    );
    let (workspace, repository) = (context.workspace.clone(), context.repository.clone());
    window
        .spawn(cx, async move |cx| {
            let result = queued.await;
            // Failed operations can still move HEAD or refs before erroring.
            refresh_branches(&repository, cx).await;
            cx.update(|window, cx| {
                if let Some((running, id)) = running {
                    running.update(cx, |running, cx| {
                        running.labels.retain(|(job_id, _)| *job_id != id);
                        cx.notify();
                    });
                }
                let Some(workspace) = workspace.upgrade() else {
                    return;
                };
                match result {
                    Ok(()) => workspace.update(cx, |workspace, cx| {
                        let id = NotificationId::unique::<RunningJobs>();
                        workspace.show_toast(Toast::new(id, operation.success).autohide(), cx);
                    }),
                    Err(error) => {
                        if !recover(&error, window, cx) {
                            show_error_toast(workspace, operation.command, error, cx)
                        }
                    }
                }
            })
            .log_err();
        })
        .detach();
}

/// The remote and the branch on it that `branch` mirrors: its upstream, or itself when remote.
pub(crate) fn fetch_target(branch: &Branch) -> Option<(String, String)> {
    let (remote, remote_branch) = if branch.is_remote() {
        branch.name().split_once('/')?
    } else {
        let upstream = branch.upstream.as_ref()?;
        (upstream.remote_name()?, upstream.branch_name()?)
    };
    // `origin/HEAD` is a symbolic ref, not a branch that can be fetched.
    (remote_branch != "HEAD").then(|| (remote.to_string(), remote_branch.to_string()))
}

/// The branch an update brings up to date, and the remote branch it takes commits from.
#[derive(Clone)]
pub(crate) struct UpdateTarget {
    pub(crate) name: String,
    pub(crate) remote: String,
    pub(crate) remote_branch: String,
    pub(crate) is_checked_out: bool,
}

pub(crate) fn update_branch(
    context: JobContext,
    repo: LocalRepo,
    target: UpdateTarget,
    options: UpdateOptions,
    window: &mut Window,
    cx: &mut App,
) {
    let operation = GitOperation {
        job: "update branch",
        progress: format!("Updating {}…", target.name),
        success: format!("Updated {}", target.name),
        command: format!("update {}", target.name),
    };
    let work = {
        let (repo, target) = (repo.clone(), target.clone());
        async move {
            git_ops::update_branch(
                &repo,
                &target.name,
                &target.remote,
                &target.remote_branch,
                target.is_checked_out,
                options,
            )
            .await
        }
    };
    let retry_context = context.clone();
    let recover = move |error: &anyhow::Error, window: &mut Window, cx: &mut App| {
        let name = target.name.clone();
        offer_update_retry(
            error,
            &name,
            options,
            window,
            cx,
            move |options, window, cx| {
                update_branch(retry_context, repo, target, options, window, cx)
            },
        )
    };
    run_git_job(&context, operation, work, recover, window, cx);
}

/// For the Git panel's pull, which fails before any of this runs since it goes through `Repository`.
pub(crate) fn recover_pull(
    error: &anyhow::Error,
    workspace: WeakEntity<Workspace>,
    repository: &Entity<Repository>,
    remote: &str,
    rebase: bool,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let Some(repo) = local_repo(repository, cx) else {
        return false;
    };
    let Some(branch) = repository.read(cx).branch.clone() else {
        return false;
    };
    let (remote, remote_branch) =
        fetch_target(&branch).unwrap_or_else(|| (remote.to_string(), branch.name().to_string()));
    let target = UpdateTarget {
        name: branch.name().to_string(),
        remote,
        remote_branch,
        is_checked_out: true,
    };
    let options = UpdateOptions {
        reconcile: rebase.then_some(Reconcile::Rebase),
        ..UpdateOptions::default()
    };
    let context = JobContext {
        workspace,
        repository: repository.clone(),
        running: None,
    };
    let name = target.name.clone();
    offer_update_retry(
        error,
        &name,
        options,
        window,
        cx,
        move |options, window, cx| update_branch(context, repo, target, options, window, cx),
    )
}

// Each blocker is offered once, so a retry that still fails ends in the error toast.
fn offer_update_retry(
    error: &anyhow::Error,
    name: &str,
    options: UpdateOptions,
    window: &mut Window,
    cx: &mut App,
    retry: impl FnOnce(UpdateOptions, &mut Window, &mut App) + 'static,
) -> bool {
    let reconcile = |reconcile| UpdateOptions {
        reconcile: Some(reconcile),
        ..options
    };
    let (message, choices) = match git_ops::blocker(error) {
        Some(Blocker::LocalChanges) if !options.autostash => (
            format!("Updating '{name}' would overwrite your local changes."),
            vec![(
                "Stash and Update",
                UpdateOptions {
                    autostash: true,
                    ..options
                },
            )],
        ),
        Some(Blocker::Diverged) if options.reconcile.is_none() => (
            format!("'{name}' and its upstream have diverged."),
            vec![
                ("Merge", reconcile(Reconcile::Merge)),
                ("Rebase", reconcile(Reconcile::Rebase)),
            ],
        ),
        _ => return false,
    };
    choose(message, changed_files(error), choices, window, cx, retry);
    true
}

/// Checks out the local branch `name`.
pub(crate) fn checkout_branch(
    context: JobContext,
    repo: LocalRepo,
    name: String,
    local_changes: LocalChanges,
    window: &mut Window,
    cx: &mut App,
) {
    let operation = GitOperation {
        job: "checkout",
        progress: format!("Checking out {name}…"),
        success: format!("Checked out {name}"),
        command: format!("checkout {name}"),
    };
    let work = {
        let (repo, name) = (repo.clone(), name.clone());
        async move { git_ops::checkout_branch(&repo, &name, local_changes).await }
    };
    let retry_context = context.clone();
    let recover = move |error: &anyhow::Error, window: &mut Window, cx: &mut App| {
        local_changes == LocalChanges::Refuse
            && offer_checkout_retry(
                error,
                &name.clone(),
                window,
                cx,
                move |changes, window, cx| {
                    checkout_branch(retry_context, repo, name, changes, window, cx)
                },
            )
    };
    run_git_job(&context, operation, work, recover, window, cx);
}

/// For a checkout that already failed through `Repository`, such as the branch picker's.
pub(crate) fn recover_checkout(
    error: &anyhow::Error,
    workspace: WeakEntity<Workspace>,
    repository: &Entity<Repository>,
    branch: &Branch,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let Some(repo) = local_repo(repository, cx) else {
        return false;
    };
    // A remote branch already got its local tracking branch before the checkout failed.
    let name = if branch.is_remote() {
        let Some((_, name)) = fetch_target(branch) else {
            return false;
        };
        name
    } else {
        branch.name().to_string()
    };
    let context = JobContext {
        workspace,
        repository: repository.clone(),
        running: None,
    };
    offer_checkout_retry(
        error,
        &name.clone(),
        window,
        cx,
        move |changes, window, cx| checkout_branch(context, repo, name, changes, window, cx),
    )
}

fn offer_checkout_retry(
    error: &anyhow::Error,
    name: &str,
    window: &mut Window,
    cx: &mut App,
    retry: impl FnOnce(LocalChanges, &mut Window, &mut App) + 'static,
) -> bool {
    if git_ops::blocker(error) != Some(Blocker::LocalChanges) {
        return false;
    }
    let choices = vec![
        ("Stash and Checkout", LocalChanges::Keep),
        ("Discard and Checkout", LocalChanges::Discard),
    ];
    let message = format!("Checking out '{name}' would overwrite your local changes.");
    choose(message, changed_files(error), choices, window, cx, retry);
    true
}

/// Asks with one button per choice plus Cancel, and hands the picked choice to `on_choice`.
pub(crate) fn choose<T: 'static>(
    message: String,
    detail: Option<String>,
    mut choices: Vec<(&'static str, T)>,
    window: &mut Window,
    cx: &mut App,
    on_choice: impl FnOnce(T, &mut Window, &mut App) + 'static,
) {
    let mut answers: Vec<&str> = choices.iter().map(|(label, _)| *label).collect();
    answers.push("Cancel");
    let answer = window.prompt(
        PromptLevel::Warning,
        &message,
        detail.as_deref(),
        &answers,
        cx,
    );
    window
        .spawn(cx, async move |cx| {
            if let Ok(index) = answer.await
                && index < choices.len()
            {
                let (_, choice) = choices.swap_remove(index);
                cx.update(|window, cx| on_choice(choice, window, cx))
                    .log_err();
            }
        })
        .detach();
}

// Git lists the files in its refusal as tab-indented lines.
fn changed_files(error: &anyhow::Error) -> Option<String> {
    let message = format!("{error:#}");
    let files: Vec<&str> = message
        .lines()
        .filter_map(|line| line.strip_prefix('\t'))
        .collect();
    (!files.is_empty()).then(|| files.join("\n"))
}

// The branch list otherwise waits for the file watcher, which can lag or miss a change.
pub(crate) async fn refresh_branches(repository: &Entity<Repository>, cx: &mut impl AppContext) {
    repository
        .update(cx, |repository, cx| repository.refresh_branches(cx))
        .await
        .map_err(|_| anyhow!("Operation was canceled"))
        .and_then(|result| result)
        .log_err();
}
