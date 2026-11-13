use super::HistoryTarget;
use crate::{
    git_graph_branches::new_branch_from_ref, git_job::GitOperation, git_text_modal::TextModal,
};
use anyhow::anyhow;
use git::repository::ResetMode;
use git_ops::Rewrite;
use git_ui_core::local_git::{local_repo, queue_git_job};
use gpui::{App, AppContext as _, ClipboardItem, PromptLevel, Window};
use std::future::Future;
use util::ResultExt as _;
use workspace::notifications::NotifyTaskExt as _;

#[derive(Clone, Copy)]
pub(super) enum ResetKind {
    Soft,
    Mixed,
    Hard,
    Keep,
}

impl HistoryTarget {
    pub(super) fn edit_message(&self, window: &mut Window, cx: &mut App) {
        let (Some(repo), Some(sha)) = (self.local(cx), self.shas.first()) else {
            return;
        };
        let (target, sha, short) = (self.clone(), sha.to_string(), sha.display_short());
        window
            .spawn(cx, async move |cx| {
                let message = load_messages(&repo, vec![sha.clone()], cx)
                    .await?
                    .pop()
                    .unwrap_or_default();
                cx.update(|window, cx| {
                    let reword = target.clone();
                    target.open_message_modal(
                        format!("Edit Commit Message ({short})"),
                        "Reword",
                        &message,
                        move |message, window, cx| {
                            let operation = operation("reword", &short, "Rewording", "Reworded");
                            let request = Rewrite::Reword {
                                sha: sha.clone(),
                                message,
                            };
                            reword.rewrite(request, sha, operation, None, window, cx)
                        },
                        window,
                        cx,
                    )
                })
            })
            .detach_and_notify_err(self.workspace.clone(), window, cx);
    }

    pub(super) fn squash(&self, window: &mut Window, cx: &mut App) {
        let Some(repo) = self.local(cx) else {
            return;
        };
        let (target, shas) = (self.clone(), self.oldest_first());
        window
            .spawn(cx, async move |cx| {
                let initial = load_messages(&repo, shas.clone(), cx).await?.join("\n\n");
                cx.update(|window, cx| {
                    let squash = target.clone();
                    target.open_message_modal(
                        format!("Squash {}", target.subject()),
                        "Squash",
                        &initial,
                        move |message, window, cx| {
                            let operation =
                                operation("squash", &squash.subject(), "Squashing", "Squashed");
                            let oldest = shas.first().cloned().unwrap_or_default();
                            let request = Rewrite::Squash {
                                shas,
                                message: Some(message),
                            };
                            squash.rewrite(request, oldest, operation, None, window, cx)
                        },
                        window,
                        cx,
                    )
                })
            })
            .detach_and_notify_err(self.workspace.clone(), window, cx);
    }

    pub(super) fn squash_into_previous(&self, window: &mut Window, cx: &mut App) {
        let (Some(sha), Some(previous)) = (self.shas.first(), self.previous) else {
            return;
        };
        let short = sha.display_short();
        let request = Rewrite::Squash {
            shas: vec![previous.to_string(), sha.to_string()],
            message: None,
        };
        let operation = operation("squash", &short, "Squashing", "Squashed");
        self.rewrite(request, previous.to_string(), operation, None, window, cx);
    }

    pub(super) fn drop_commits(&self, window: &mut Window, cx: &mut App) {
        let subject = self.subject();
        let confirm = (
            format!("Drop {subject}?"),
            "They are removed from the branch but stay reachable through the reflog.".to_string(),
        );
        let shas = self.oldest_first();
        let Some(oldest) = shas.first().cloned() else {
            return;
        };
        let request = Rewrite::Drop { shas };
        let operation = operation("drop", &subject, "Dropping", "Dropped");
        self.rewrite(request, oldest, operation, Some(confirm), window, cx);
    }

    pub(super) fn revert(&self, window: &mut Window, cx: &mut App) {
        let Some(repo) = self.local(cx) else {
            return;
        };
        let shas: Vec<String> = self.shas.iter().map(ToString::to_string).collect();
        let operation = operation("revert", &self.subject(), "Reverting", "Reverted");
        self.run(operation, window, cx, async move {
            git_ops::revert(&repo, &shas).await
        });
    }

    pub(super) fn cherry_pick(&self, window: &mut Window, cx: &mut App) {
        let Some(repo) = self.local(cx) else {
            return;
        };
        let shas = self.oldest_first();
        let operation = operation(
            "cherry-pick",
            &self.subject(),
            "Cherry-picking",
            "Cherry-picked",
        );
        self.run(operation, window, cx, async move {
            git_ops::cherry_pick(&repo, &shas).await
        });
    }

    pub(super) fn reset(&self, kind: ResetKind, window: &mut Window, cx: &mut App) {
        let Some(sha) = self.shas.first() else {
            return;
        };
        let (short, sha) = (sha.display_short(), sha.to_string());
        let native_mode = match kind {
            ResetKind::Soft => Some(ResetMode::Soft),
            ResetKind::Mixed => Some(ResetMode::Mixed),
            ResetKind::Hard | ResetKind::Keep => None,
        };
        if let Some(mode) = native_mode {
            let receiver = self
                .repository
                .update(cx, |repository, cx| repository.reset(sha, mode, cx));
            cx.spawn(async move |_| {
                receiver
                    .await
                    .map_err(|_| anyhow!("Operation was canceled"))?
            })
            .detach_and_notify_err(self.workspace.clone(), window, cx);
            return;
        }

        let Some(repo) = self.local(cx) else {
            return;
        };
        let mode = match kind {
            ResetKind::Hard => git_ops::ResetMode::Hard,
            _ => git_ops::ResetMode::Keep,
        };
        let operation = operation("reset", &short, "Resetting to", "Reset to");
        let work = async move { git_ops::reset(&repo, &sha, mode).await };
        match mode {
            git_ops::ResetMode::Keep => self.run(operation, window, cx, work),
            git_ops::ResetMode::Hard => {
                let answer = window.prompt(
                    PromptLevel::Warning,
                    &format!("Hard reset the branch to {short}?"),
                    Some("Uncommitted changes in tracked files are discarded for good."),
                    &["Reset", "Cancel"],
                    cx,
                );
                let target = self.clone();
                window
                    .spawn(cx, async move |cx| {
                        if answer.await == Ok(0) {
                            cx.update(|window, cx| target.run(operation, window, cx, work))?;
                        }
                        anyhow::Ok(())
                    })
                    .detach_and_notify_err(self.workspace.clone(), window, cx);
            }
        }
    }

    pub(super) fn new_branch(&self, window: &mut Window, cx: &mut App) {
        let Some(sha) = self.shas.first() else {
            return;
        };
        new_branch_from_ref(
            &self.workspace,
            self.repository.clone(),
            sha.to_string(),
            sha.display_short(),
            window,
            cx,
        );
    }

    pub(super) fn copy_shas(&self, cx: &mut App) {
        let shas: Vec<String> = self.shas.iter().map(ToString::to_string).collect();
        cx.write_to_clipboard(ClipboardItem::new_string(shas.join("\n")));
    }

    fn local(&self, cx: &mut App) -> Option<git_ops::LocalRepo> {
        local_repo(&self.repository, cx)
    }

    fn oldest_first(&self) -> Vec<String> {
        self.shas.iter().rev().map(ToString::to_string).collect()
    }

    fn subject(&self) -> String {
        match self.shas.as_ref() {
            [sha] => sha.display_short(),
            shas => format!("{} commits", shas.len()),
        }
    }

    // Rewrites also warn when the commits were already pushed, since that needs a force push.
    fn rewrite(
        &self,
        request: Rewrite,
        oldest: String,
        operation: GitOperation,
        confirm: Option<(String, String)>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(repo) = self.local(cx) else {
            return;
        };
        let target = self.clone();
        window
            .spawn(cx, async move |cx| {
                let pushed = cx
                    .background_spawn({
                        let repo = repo.clone();
                        async move { git_ops::pushed_to(&repo, &oldest).await }
                    })
                    .await
                    .log_err()
                    .flatten();
                let mut details: Vec<String> =
                    confirm.iter().map(|(_, detail)| detail.clone()).collect();
                details.extend(pushed.map(|remote| {
                    format!("This history is already on {remote}, so it will need a force push.")
                }));
                if confirm.is_some() || !details.is_empty() {
                    let question = confirm
                        .map_or("Rewrite pushed history?".to_string(), |(question, _)| {
                            question
                        });
                    let answer = cx.update(|window, cx| {
                        let detail = details.join("\n\n");
                        window.prompt(
                            PromptLevel::Warning,
                            &question,
                            Some(&detail),
                            &["Continue", "Cancel"],
                            cx,
                        )
                    })?;
                    if answer.await != Ok(0) {
                        return anyhow::Ok(());
                    }
                }
                cx.update(|window, cx| {
                    target.run(operation, window, cx, async move {
                        git_ops::rewrite(&repo, request).await
                    })
                })
            })
            .detach_and_notify_err(self.workspace.clone(), window, cx);
    }

    // The sidebar shows progress in its footer; without it the job still runs, just unannounced.
    fn run(
        &self,
        operation: GitOperation,
        window: &mut Window,
        cx: &mut App,
        work: impl Future<Output = anyhow::Result<()>> + Send + 'static,
    ) {
        match self.sidebar.upgrade() {
            Some(sidebar) => sidebar.update(cx, |sidebar, cx| {
                sidebar.run_git_operation(&self.repository, operation, work, window, cx)
            }),
            None => queue_git_job(
                &self.repository,
                operation.job,
                operation.progress,
                work,
                cx,
            )
            .detach_and_notify_err(self.workspace.clone(), window, cx),
        }
    }

    fn open_message_modal(
        &self,
        title: String,
        confirm_label: &'static str,
        initial: &str,
        on_confirm: impl FnOnce(String, &mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    TextModal::multi_line(
                        title.into(),
                        confirm_label,
                        initial,
                        Box::new(on_confirm),
                        window,
                        cx,
                    )
                });
            })
            .log_err();
    }
}

fn operation(job: &'static str, subject: &str, progress: &str, success: &str) -> GitOperation {
    GitOperation {
        job,
        progress: format!("{progress} {subject}…"),
        success: format!("{success} {subject}"),
        command: format!("{job} {subject}"),
    }
}

async fn load_messages(
    repo: &git_ops::LocalRepo,
    shas: Vec<String>,
    cx: &mut gpui::AsyncWindowContext,
) -> anyhow::Result<Vec<String>> {
    let repo = repo.clone();
    cx.background_spawn(async move { git_ops::commit_messages(&repo, &shas).await })
        .await
}
