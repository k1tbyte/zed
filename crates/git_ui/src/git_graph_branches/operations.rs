use super::BranchesSidebar;
use crate::git_graph_push::{PushMode, PushRequest, PushView};
use crate::git_job::{
    self, GitOperation, UpdateTarget, fetch_target, refresh_branches, run_git_job,
};
use crate::git_text_modal::TextModal;
use anyhow::anyhow;
use futures::channel::oneshot;
use git::repository::Branch;
use git_ops::{LocalChanges, UpdateOptions};
use git_ui_core::local_git::local_repo;
use gpui::{App, Context, Entity, WeakEntity, Window};
use project::git_store::Repository;
use std::future::Future;
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{Workspace, notifications::DetachAndPromptErr as _};

impl BranchesSidebar {
    pub(super) fn new_branch_from(
        &mut self,
        base: &Branch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        let base = base.name().to_string();
        new_branch_from_ref(&self.workspace, repository, base.clone(), base, window, cx);
    }

    pub(super) fn rename(&mut self, branch: &Branch, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        let old_name = branch.name().to_string();
        TextModal::open_single_line(
            &self.workspace,
            format!("Rename Branch ({old_name})"),
            IconName::GitBranch,
            &old_name.clone(),
            window,
            cx,
            move |new_name, window, cx| {
                if new_name != old_name {
                    run_branch_operation(
                        repository,
                        "Failed to rename branch",
                        window,
                        cx,
                        move |repo| repo.rename_branch(old_name, new_name),
                    );
                }
            },
        );
    }

    pub(super) fn fetch(&mut self, branch: &Branch, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        let Some((remote, remote_branch)) = fetch_target(branch) else {
            return;
        };
        let Some(repo) = local_repo(&repository, cx) else {
            return;
        };
        let name = branch.name().to_string();
        self.run_git_operation(
            &repository,
            GitOperation {
                job: "fetch branch",
                progress: format!("Fetching {name}…"),
                success: format!("Fetched {name}"),
                command: format!("fetch {name}"),
            },
            async move { git_ops::fetch_branch(&repo, &remote, &remote_branch).await },
            window,
            cx,
        );
    }

    pub(super) fn current_branch(&self, cx: &App) -> Option<Branch> {
        let repository = self.repository(cx)?;
        let repository = repository.read(cx);
        repository
            .branch_list
            .iter()
            .find(|branch| branch.is_head && !branch.is_remote())
            .cloned()
    }

    pub(super) fn rebase_current_onto(
        &mut self,
        target: &Branch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(repository), Some(current)) = (self.repository(cx), self.current_branch(cx))
        else {
            return;
        };
        let Some(repo) = local_repo(&repository, cx) else {
            return;
        };
        let (current_name, target_name) = (current.name().to_string(), target.name().to_string());
        let onto = target.ref_name.to_string();
        self.run_git_operation(
            &repository,
            GitOperation {
                job: "rebase",
                progress: format!("Rebasing {current_name} onto {target_name}…"),
                success: format!("Rebased {current_name} onto {target_name}"),
                command: format!("rebase {target_name}"),
            },
            async move { git_ops::rebase_onto(&repo, &onto).await },
            window,
            cx,
        );
    }

    pub(super) fn push(&mut self, branch: &Branch, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        let Some(repo) = local_repo(&repository, cx) else {
            return;
        };
        let branch = branch.clone();
        let workspace = self.workspace.clone();
        // Strong on purpose: the Push tab can outlive the graph, and the push must still run.
        let sidebar = cx.entity();
        cx.spawn_in(window, async move |_, cx| {
            let upstream = branch.upstream.as_ref().and_then(|upstream| {
                Some((upstream, upstream.remote_name()?, upstream.branch_name()?))
            });
            let (remote, remote_branch, upstream_ref, set_upstream) = match upstream {
                Some((upstream, remote, remote_branch)) => (
                    remote.to_string(),
                    remote_branch.to_string(),
                    branch.tracking_status().map(|_| upstream.ref_name.clone()),
                    false,
                ),
                None => {
                    let remote = cx
                        .background_spawn({
                            let repo = repo.clone();
                            async move { git_ops::default_remote(&repo).await }
                        })
                        .await
                        .log_err()
                        .unwrap_or_else(|| "origin".to_string());
                    (remote, branch.name().to_string(), None, true)
                }
            };
            let request = PushRequest {
                branch: branch.name().to_string().into(),
                remote: remote.into(),
                remote_branch: remote_branch.into(),
                set_upstream,
                upstream_ref,
                repo,
                repository: repository.downgrade(),
                workspace,
            };
            let on_confirm = Box::new(
                move |request: &PushRequest, mode: PushMode, window: &mut Window, cx: &mut App| {
                    let Some(repository) = request.repository.upgrade() else {
                        return;
                    };
                    let (repo, name, remote) = (
                        request.repo.clone(),
                        request.branch.clone(),
                        request.remote.clone(),
                    );
                    let (remote_branch, set_upstream) =
                        (request.remote_branch.clone(), request.set_upstream);
                    let operation = GitOperation {
                        job: "push",
                        progress: format!("Pushing {name} to {remote}…"),
                        success: format!("Pushed {name} to {remote}"),
                        command: format!("push {name}"),
                    };
                    let work = async move {
                        let target = git_ops::PushTarget {
                            remote: &remote,
                            branch: &remote_branch,
                            set_upstream,
                            force_with_lease: matches!(mode, PushMode::ForceWithLease),
                        };
                        git_ops::push_branch(&repo, &name, target).await
                    };
                    sidebar.update(cx, |sidebar, cx| {
                        sidebar.run_git_operation(&repository, operation, work, window, cx)
                    });
                },
            );
            cx.update(|window, cx| PushView::open(request, on_confirm, window, cx))
        })
        .detach_and_log_err(cx);
    }

    // Progress is shown in the sidebar footer, the outcome as a toast.
    pub(crate) fn run_git_operation(
        &self,
        repository: &Entity<Repository>,
        operation: GitOperation,
        work: impl Future<Output = anyhow::Result<()>> + Send + 'static,
        window: &mut Window,
        cx: &mut App,
    ) {
        let context = self.job_context(repository.clone());
        let no_recovery = |_: &anyhow::Error, _: &mut Window, _: &mut App| false;
        run_git_job(&context, operation, work, no_recovery, window, cx);
    }

    pub(super) fn update(&mut self, branch: &Branch, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        let (Some((remote, remote_branch)), Some(repo)) =
            (fetch_target(branch), local_repo(&repository, cx))
        else {
            return;
        };
        let target = UpdateTarget {
            name: branch.name().to_string(),
            remote,
            remote_branch,
            is_checked_out: branch.is_head,
        };
        let context = self.job_context(repository);
        git_job::update_branch(context, repo, target, UpdateOptions::default(), window, cx);
    }

    pub(super) fn checkout(
        &mut self,
        branch: &Branch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        let name = branch.name().to_string();
        match local_repo(&repository, cx) {
            Some(repo) => {
                let context = self.job_context(repository);
                git_job::checkout_branch(context, repo, name, LocalChanges::Refuse, window, cx);
            }
            // Remote projects have no checkout on this machine to stash in.
            None => run_branch_operation(
                repository,
                "Failed to checkout branch",
                window,
                cx,
                move |repository| repository.change_branch(name),
            ),
        }
    }
}

/// Asks for a name, then creates the branch at `base`. `label` is what the title shows.
pub(crate) fn new_branch_from_ref(
    workspace: &WeakEntity<Workspace>,
    repository: Entity<Repository>,
    base: String,
    label: String,
    window: &mut Window,
    cx: &mut App,
) {
    TextModal::open_single_line(
        workspace,
        format!("New Branch from {label}"),
        IconName::GitBranch,
        "",
        window,
        cx,
        move |name, window, cx| {
            run_branch_operation(
                repository,
                "Failed to create branch",
                window,
                cx,
                move |repo| repo.create_branch(name, Some(base)),
            );
        },
    );
}

pub(super) fn run_branch_operation(
    repository: Entity<Repository>,
    error_title: &str,
    window: &mut Window,
    cx: &mut App,
    operation: impl FnOnce(&mut Repository) -> oneshot::Receiver<anyhow::Result<()>> + 'static,
) {
    cx.spawn(async move |cx| {
        let result = match repository
            .update(cx, |repository, _| operation(repository))
            .await
        {
            Ok(result) => result,
            Err(_) => Err(anyhow!("Operation was canceled")),
        };
        if result.is_ok() {
            refresh_branches(&repository, cx).await;
        }
        result
    })
    .detach_and_prompt_err(error_title, window, cx, |_, _, _| None);
}
