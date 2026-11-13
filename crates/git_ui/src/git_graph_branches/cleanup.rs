use super::BranchesSidebar;
use crate::branch_picker::{delete_branch_command, force_delete_prompt_for_branch_delete_error};
use crate::git_job::{GitOperation, choose, fetch_target, refresh_branches};
use anyhow::{Context as _, anyhow};
use git::repository::Branch;
use git_ops::LocalRepo;
use git_ui_core::local_git::local_repo;
use git_ui_core::notifications::show_error_toast;
use gpui::{
    AppContext as _, AsyncWindowContext, Context, Entity, PromptLevel, TaskExt as _, WeakEntity,
    Window,
};
use project::git_store::Repository;
use std::collections::HashSet;
use workspace::notifications::{DetachAndPromptErr as _, NotificationId};
use workspace::{Toast, Workspace};

impl BranchesSidebar {
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
        let local = local_repo(&repository, cx);
        // Remote projects can only drop the tracking ref, which the next fetch brings back.
        let (on_remote, refs): (Vec<Branch>, Vec<Branch>) = branches
            .into_iter()
            .filter(|branch| !branch.is_head)
            .partition(|branch| {
                local.is_some() && branch.is_remote() && fetch_target(branch).is_some()
            });
        let workspace = self.workspace.clone();
        let refs: Vec<(bool, String)> = refs
            .iter()
            .map(|branch| (branch.is_remote(), branch.name().to_string()))
            .collect();
        // Sequential because a second GPUI prompt replaces and cancels the first one.
        cx.spawn_in(window, async move |this, cx| {
            if !refs.is_empty() {
                delete_refs(&repository, refs, workspace, cx).await?;
            }
            if !on_remote.is_empty()
                && let Some(repo) = local
            {
                this.update_in(cx, |this, window, cx| {
                    this.delete_from_remotes(&repository, repo, &on_remote, window, cx)
                })?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn delete_from_remotes(
        &mut self,
        repository: &Entity<Repository>,
        repo: LocalRepo,
        branches: &[Branch],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let targets: Vec<(String, String)> = branches.iter().filter_map(fetch_target).collect();
        let (message, label) = match targets.as_slice() {
            [] => return,
            [(remote, branch)] => (
                format!("Delete '{branch}' from {remote}?"),
                format!("{remote}/{branch}"),
            ),
            _ => (
                format!("Delete {} branches from their remotes?", targets.len()),
                format!("{} remote branches", targets.len()),
            ),
        };
        let names: Vec<&str> = branches.iter().map(Branch::name).collect();
        let (sidebar, repository) = (cx.weak_entity(), repository.clone());
        choose(
            message,
            Some(names.join("\n")),
            vec![("Delete", ())],
            window,
            cx,
            move |(), window, cx| {
                let operation = GitOperation {
                    job: "delete remote branch",
                    progress: format!("Deleting {label}…"),
                    success: format!("Deleted {label}"),
                    command: "push --delete".to_string(),
                };
                let work = async move {
                    for (remote, branch) in targets {
                        git_ops::delete_remote_branch(&repo, &remote, &branch).await?;
                    }
                    Ok(())
                };
                sidebar
                    .update(cx, |sidebar, cx| {
                        sidebar.run_git_operation(&repository, operation, work, window, cx)
                    })
                    .ok();
            },
        );
    }

    pub(super) fn prune_remote_branches(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        let Some(repo) = local_repo(&repository, cx) else {
            return;
        };
        let operation = GitOperation {
            job: "prune",
            progress: "Pruning remote branches…".to_string(),
            success: "Pruned remote branches".to_string(),
            command: "fetch --prune".to_string(),
        };
        let work = async move { git_ops::prune_remote_branches(&repo).await };
        self.run_git_operation(&repository, operation, work, window, cx);
    }

    /// Offers the local branches merged into the default branch or whose upstream was deleted.
    pub(super) fn delete_merged_branches(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        let Some(repo) = local_repo(&repository, cx) else {
            return;
        };
        let default_branch = repository.update(cx, |repository, _| repository.default_branch(true));
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |this, cx| {
            let target = default_branch
                .await
                .map_err(|_| anyhow!("Operation was canceled"))??
                .context("No default branch to compare with")?;
            let merged: HashSet<String> = cx
                .background_spawn({
                    let target = target.to_string();
                    async move { git_ops::merged_branches(&repo, &target).await }
                })
                .await?
                .into_iter()
                .collect();
            // The target is usually `origin/main`, whose local copy is no cleanup candidate either.
            let local_default = target.split_once('/').map_or(&*target, |(_, name)| name);
            let candidates: Vec<Branch> = repository.read_with(cx, |repository, _| {
                repository
                    .branch_list
                    .iter()
                    .filter(|branch| {
                        !branch.is_remote()
                            && !branch.is_head
                            && ![local_default, &*target].contains(&branch.name())
                            && (merged.contains(branch.ref_name.as_ref())
                                || branch
                                    .upstream
                                    .as_ref()
                                    .is_some_and(|upstream| upstream.tracking.is_gone()))
                    })
                    .cloned()
                    .collect()
            });
            cx.update(|window, cx| {
                if candidates.is_empty() {
                    if let Some(workspace) = workspace.upgrade() {
                        workspace.update(cx, |workspace, cx| {
                            let id = NotificationId::unique::<BranchesSidebar>();
                            let toast = Toast::new(id, "No merged branches to delete");
                            workspace.show_toast(toast.autohide(), cx);
                        });
                    }
                    return;
                }
                let names: Vec<&str> = candidates.iter().map(Branch::name).collect();
                choose(
                    format!("Delete {} merged branches?", candidates.len()),
                    Some(names.join("\n")),
                    vec![("Delete", ())],
                    window,
                    cx,
                    move |(), window, cx| {
                        this.update(cx, |this, cx| this.delete_branches(candidates, window, cx))
                            .ok();
                    },
                );
            })
        })
        .detach_and_prompt_err("Failed to find merged branches", window, cx, |_, _, _| None);
    }
}

async fn delete_refs(
    repository: &Entity<Repository>,
    branches: Vec<(bool, String)>,
    workspace: WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
) -> anyhow::Result<()> {
    let mut unmerged = Vec::new();
    let mut failures = Vec::new();
    for (is_remote, name) in branches {
        if let Err(error) = delete_one(repository, is_remote, &name, false, cx).await {
            match force_delete_prompt_for_branch_delete_error(&error, &name) {
                Some(prompt) => unmerged.push((is_remote, name, prompt)),
                None => failures.push((delete_branch_command(is_remote, &name, false), error)),
            }
        }
    }
    if !unmerged.is_empty() {
        let prompt = match unmerged.as_slice() {
            [(_, _, prompt)] => prompt.clone(),
            _ => {
                let names: Vec<&str> = unmerged.iter().map(|(_, name, _)| name.as_str()).collect();
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
                if let Err(error) = delete_one(repository, is_remote, &name, true, cx).await {
                    failures.push((delete_branch_command(is_remote, &name, true), error));
                }
            }
        }
    }
    refresh_branches(repository, cx).await;
    if let Some((command, error)) = failures.into_iter().next()
        && let Some(workspace) = workspace.upgrade()
    {
        cx.update(|_, cx| show_error_toast(workspace, command, error, cx))?;
    }
    Ok(())
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
