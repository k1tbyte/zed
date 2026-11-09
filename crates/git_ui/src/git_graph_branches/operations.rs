use super::BranchesSidebar;
use crate::branch_picker::{delete_branch_command, force_delete_prompt_for_branch_delete_error};
use crate::git_graph_push::{PushMode, PushRequest, PushView};
use crate::git_text_modal::TextModal;
use crate::local_git::{local_repo, queue_git_job};
use anyhow::anyhow;
use futures::channel::oneshot;
use git::repository::Branch;
use git_ui_core::notifications::show_error_toast;
use gpui::{App, AsyncWindowContext, Context, Entity, PromptLevel, WeakEntity, Window};
use project::git_store::Repository;
use std::future::Future;
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{
    Toast, Workspace,
    notifications::{DetachAndPromptErr as _, NotificationId},
};

pub(crate) struct GitOperation {
    pub(crate) job: &'static str,
    pub(crate) progress: String,
    pub(crate) success: String,
    pub(crate) command: String,
}

impl BranchesSidebar {
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
        run_branch_operation(
            repository,
            "Failed to checkout branch",
            window,
            cx,
            move |repo| repo.change_branch(name),
        );
    }

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

    pub(super) fn delete(&mut self, branch: &Branch, window: &mut Window, cx: &mut Context<Self>) {
        self.delete_branches(vec![branch.clone()], window, cx);
    }

    // Unmerged branches are collected so the user gets a single force-delete prompt.
    pub(super) fn delete_branches(
        &mut self,
        branches: Vec<Branch>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        let workspace = self.workspace.clone();
        let branches: Vec<(bool, String)> = branches
            .iter()
            .filter(|branch| !branch.is_head)
            .map(|branch| (branch.is_remote(), branch.name().to_string()))
            .collect();
        cx.spawn_in(window, async move |_, cx| {
            let mut unmerged = Vec::new();
            let mut failures = Vec::new();
            for (is_remote, name) in branches {
                if let Err(error) = delete_one(&repository, is_remote, &name, false, cx).await {
                    match force_delete_prompt_for_branch_delete_error(&error, &name) {
                        Some(prompt) => unmerged.push((is_remote, name, prompt)),
                        None => {
                            failures.push((delete_branch_command(is_remote, &name, false), error))
                        }
                    }
                }
            }
            if !unmerged.is_empty() {
                let prompt = match unmerged.as_slice() {
                    [(_, _, prompt)] => prompt.clone(),
                    _ => {
                        let names: Vec<&str> =
                            unmerged.iter().map(|(_, name, _)| name.as_str()).collect();
                        format!(
                            "{} branches are not fully merged: {}. Force delete them?",
                            names.len(),
                            names.join(", ")
                        )
                    }
                };
                let answer = cx.update(|window, cx| {
                    window.prompt(
                        PromptLevel::Warning,
                        &prompt,
                        None,
                        &["Force Delete", "Cancel"],
                        cx,
                    )
                })?;
                if answer.await == Ok(0) {
                    for (is_remote, name, _) in unmerged {
                        if let Err(error) =
                            delete_one(&repository, is_remote, &name, true, cx).await
                        {
                            failures.push((delete_branch_command(is_remote, &name, true), error));
                        }
                    }
                }
            }
            if let Some((command, error)) = failures.into_iter().next()
                && let Some(workspace) = workspace.upgrade()
            {
                cx.update(|_, cx| show_error_toast(workspace, command, error, cx))?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    pub(super) fn update(&mut self, branch: &Branch, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        let Some(upstream) = branch.upstream.as_ref() else {
            return;
        };
        let (Some(remote), Some(remote_branch)) = (upstream.remote_name(), upstream.branch_name())
        else {
            return;
        };
        let Some(repo) = local_repo(&repository, cx) else {
            return;
        };
        let (remote, remote_branch) = (remote.to_string(), remote_branch.to_string());
        let name = branch.name().to_string();
        let is_checked_out = branch.is_head;
        self.run_git_operation(
            &repository,
            GitOperation {
                job: "update branch",
                progress: format!("Updating {name}…"),
                success: format!("Updated {name}"),
                command: format!("update {name}"),
            },
            async move {
                git_ops::update_branch(&repo, &name, &remote, &remote_branch, is_checked_out).await
            },
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
        &mut self,
        repository: &Entity<Repository>,
        operation: GitOperation,
        work: impl Future<Output = anyhow::Result<()>> + Send + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.next_operation_id;
        self.next_operation_id += 1;
        self.operations
            .push((id, operation.progress.clone().into()));
        cx.notify();

        let queued = queue_git_job(
            repository,
            operation.job,
            operation.progress.clone(),
            work,
            cx,
        );
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = queued.await;
            // The result below must still be reported when the graph was closed meanwhile.
            this.update(cx, |this, cx| {
                this.operations
                    .retain(|(operation_id, _)| *operation_id != id);
                cx.notify();
            })
            .ok();
            cx.update(|_, cx| {
                let Some(workspace) = workspace.upgrade() else {
                    return;
                };
                match result {
                    Ok(()) => workspace.update(cx, |workspace, cx| {
                        let id = NotificationId::unique::<BranchesSidebar>();
                        workspace.show_toast(Toast::new(id, operation.success).autohide(), cx);
                    }),
                    Err(error) => show_error_toast(workspace, operation.command, error, cx),
                }
            })
        })
        .detach_and_log_err(cx);
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

async fn delete_one(
    repository: &Entity<Repository>,
    is_remote: bool,
    name: &str,
    force: bool,
    cx: &mut AsyncWindowContext,
) -> anyhow::Result<()> {
    repository
        .update(cx, |repository, _| {
            repository.delete_branch(is_remote, name.to_string(), force)
        })
        .await
        .map_err(|_| anyhow!("Operation was canceled"))?
}

fn run_branch_operation(
    repository: Entity<Repository>,
    error_title: &str,
    window: &mut Window,
    cx: &mut App,
    operation: impl FnOnce(&mut Repository) -> oneshot::Receiver<anyhow::Result<()>> + 'static,
) {
    cx.spawn(async move |cx| {
        match repository
            .update(cx, |repository, _| operation(repository))
            .await
        {
            Ok(result) => result,
            Err(_) => Err(anyhow!("Operation was canceled")),
        }
    })
    .detach_and_prompt_err(error_title, window, cx, |_, _, _| None);
}
