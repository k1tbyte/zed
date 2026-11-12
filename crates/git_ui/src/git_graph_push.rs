use crate::commit_view::{CommitView, CommitViewOptions};
use git::repository::RepoPath;
use git_ops::{ChangedFile, OutgoingCommit};
use gpui::{
    AnyElement, App, Context, EventEmitter, FocusHandle, Focusable, SharedString, Task, WeakEntity,
    Window,
};
use project::git_store::Repository;
use std::collections::HashMap;
use theme::ActiveTheme as _;
use ui::{
    Button, ButtonStyle, Headline, HeadlineSize, ListItem, ListItemSpacing, TintColor, Tooltip,
    prelude::*,
};
use util::ResultExt as _;
use workspace::{
    Workspace,
    item::{Item, ItemEvent, ItemHandle as _},
};

pub(crate) struct PushRequest {
    pub(crate) branch: SharedString,
    pub(crate) remote: SharedString,
    pub(crate) remote_branch: SharedString,
    pub(crate) set_upstream: bool,
    pub(crate) upstream_ref: Option<SharedString>,
    pub(crate) repo: git_ops::LocalRepo,
    pub(crate) repository: WeakEntity<Repository>,
    pub(crate) workspace: WeakEntity<Workspace>,
}

#[derive(Clone, Copy)]
pub(crate) enum PushMode {
    Normal,
    ForceWithLease,
}

enum Commits {
    Loading,
    Loaded(Vec<OutgoingCommit>),
    Failed(SharedString),
}

type ConfirmCallback = Box<dyn FnOnce(&PushRequest, PushMode, &mut Window, &mut App)>;

pub(crate) enum PushEvent {
    Close,
}

pub(crate) struct PushView {
    request: PushRequest,
    commits: Commits,
    expanded: Option<usize>,
    files: HashMap<String, Vec<ChangedFile>>,
    focus_handle: FocusHandle,
    on_confirm: Option<ConfirmCallback>,
    _tasks: Vec<Task<()>>,
}

impl PushView {
    pub(crate) fn open(
        request: PushRequest,
        on_confirm: ConfirmCallback,
        window: &mut Window,
        cx: &mut App,
    ) {
        let workspace = request.workspace.clone();
        workspace
            .update(cx, |workspace, cx| {
                let (branch, dir) = (request.branch.clone(), request.repo.dir.clone());
                let view = cx.new(|cx| PushView::new(request, on_confirm, cx));
                workspace.active_pane().update(cx, |pane, cx| {
                    let stale = pane.items().enumerate().find_map(|(ix, item)| {
                        let existing = item.downcast::<PushView>()?;
                        let request = &existing.read(cx).request;
                        (request.branch == branch && request.repo.dir == dir)
                            .then_some((ix, existing))
                    });
                    let ix = stale.map(|(ix, existing)| {
                        pane.remove_item(existing.item_id(), false, false, window, cx);
                        ix
                    });
                    pane.add_item(Box::new(view), true, true, ix, window, cx);
                });
            })
            .log_err();
    }

    fn new(request: PushRequest, on_confirm: ConfirmCallback, cx: &mut Context<Self>) -> Self {
        let repo = request.repo.clone();
        let branch_ref = format!("refs/heads/{}", request.branch);
        let upstream_ref = request.upstream_ref.clone();
        let load = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    git_ops::outgoing_commits(&repo, &branch_ref, upstream_ref.as_deref()).await
                })
                .await;
            this.update(cx, |this, cx| {
                this.commits = match result {
                    Ok(commits) => Commits::Loaded(commits),
                    Err(error) => Commits::Failed(error.to_string().into()),
                };
                cx.notify();
            })
            .ok();
        });

        Self {
            request,
            commits: Commits::Loading,
            expanded: None,
            files: HashMap::new(),
            focus_handle: cx.focus_handle(),
            on_confirm: Some(on_confirm),
            _tasks: vec![load],
        }
    }

    // A forced push may also publish a reset, where no commit is outgoing.
    fn can_push(&self, mode: PushMode) -> bool {
        match (&self.commits, mode) {
            (Commits::Loaded(_), PushMode::ForceWithLease) => true,
            (Commits::Loaded(commits), PushMode::Normal) => {
                !commits.is_empty() || self.request.set_upstream
            }
            _ => false,
        }
    }

    fn push(&mut self, mode: PushMode, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_push(mode) {
            return;
        }
        if let Some(on_confirm) = self.on_confirm.take() {
            on_confirm(&self.request, mode, window, cx);
        }
        cx.emit(PushEvent::Close);
    }

    fn toggle_commit(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.expanded == Some(ix) {
            self.expanded = None;
            cx.notify();
            return;
        }
        self.expanded = Some(ix);
        cx.notify();

        let Commits::Loaded(commits) = &self.commits else {
            return;
        };
        let Some(commit) = commits.get(ix) else {
            return;
        };
        if self.files.contains_key(&commit.sha) {
            return;
        }
        let sha = commit.sha.clone();
        let repo = self.request.repo.clone();
        let load = cx.spawn(async move |this, cx| {
            let files = cx
                .background_spawn({
                    let sha = sha.clone();
                    async move { git_ops::commit_files(&repo, &sha).await }
                })
                .await
                .log_err()
                .unwrap_or_default();
            this.update(cx, |this, cx| {
                this.files.insert(sha, files);
                cx.notify();
            })
            .ok();
        });
        self._tasks.push(load);
    }

    fn open_file_diff(
        &mut self,
        sha: &str,
        path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        CommitView::open(
            sha.to_string(),
            self.request.repository.clone(),
            self.request.workspace.clone(),
            CommitViewOptions {
                file_filter: RepoPath::new(path).log_err(),
                ..Default::default()
            },
            window,
            cx,
        );
    }

    fn render_commits(
        &self,
        commits: &[OutgoingCommit],
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut rows = Vec::new();
        for (ix, commit) in commits.iter().enumerate() {
            let is_expanded = self.expanded == Some(ix);
            let chevron = if is_expanded {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            };
            let short_sha: String = commit.sha.chars().take(8).collect();
            rows.push(
                ListItem::new(("outgoing-commit", ix))
                    .spacing(ListItemSpacing::Sparse)
                    .toggle_state(is_expanded)
                    .start_slot(
                        Icon::new(chevron)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        h_flex()
                            .min_w_0()
                            .gap_2()
                            .child(
                                Label::new(short_sha)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new(commit.subject.clone())
                                    .size(LabelSize::Small)
                                    .truncate(),
                            ),
                    )
                    .end_slot(
                        Label::new(commit.author.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_commit(ix, cx)))
                    .into_any_element(),
            );
            if !is_expanded {
                continue;
            }
            for (file_ix, file) in self
                .files
                .get(&commit.sha)
                .into_iter()
                .flatten()
                .enumerate()
            {
                let color = match file.status {
                    'A' => Color::Created,
                    'D' => Color::Deleted,
                    _ => Color::Modified,
                };
                let (sha, path) = (commit.sha.clone(), file.path.clone());
                rows.push(
                    ListItem::new(("outgoing-file", ix * 10_000 + file_ix))
                        .spacing(ListItemSpacing::Sparse)
                        .indent_level(1)
                        .start_slot(
                            Label::new(file.status.to_string())
                                .size(LabelSize::Small)
                                .color(color),
                        )
                        .child(Label::new(file.path.clone()).size(LabelSize::Small))
                        .tooltip(Tooltip::text("Open diff"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_file_diff(&sha, &path, window, cx)
                        }))
                        .into_any_element(),
                );
            }
        }
        rows
    }
}

impl EventEmitter<PushEvent> for PushView {}
impl Focusable for PushView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for PushView {
    type Event = PushEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        format!("Push {}", self.request.branch).into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ArrowUp).color(Color::Muted))
    }

    fn to_item_events(event: &PushEvent, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            PushEvent::Close => f(ItemEvent::CloseItem),
        }
    }
}

impl Render for PushView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let border_color = cx.theme().colors().border_variant;
        let (can_push, can_force_push) = (
            self.can_push(PushMode::Normal),
            self.can_push(PushMode::ForceWithLease),
        );
        let body = match &self.commits {
            Commits::Loading => vec![placeholder("Loading outgoing commits…", Color::Muted)],
            Commits::Failed(error) => vec![placeholder(error.clone(), Color::Error)],
            Commits::Loaded(commits) if commits.is_empty() => {
                let message = if self.request.set_upstream {
                    "No commits yet. Pushing will create the branch on the remote."
                } else {
                    "No outgoing commits."
                };
                vec![placeholder(message, Color::Muted)]
            }
            Commits::Loaded(commits) => self.render_commits(commits, cx),
        };

        v_flex()
            .key_context("PushView")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(
                h_flex()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .gap_1p5()
                    .child(Icon::new(IconName::ArrowUp).size(IconSize::XSmall))
                    .child(
                        Headline::new(format!(
                            "Push {} to {}/{}",
                            self.request.branch, self.request.remote, self.request.remote_branch
                        ))
                        .size(HeadlineSize::XSmall),
                    ),
            )
            .child(
                div()
                    .id("push-commits")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .border_y_1()
                    .border_color(border_color)
                    .children(body),
            )
            .child(
                h_flex()
                    .p_2()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("push-cancel", "Cancel")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(PushEvent::Close))),
                    )
                    .child(
                        Button::new("push-force", "Force Push")
                            .style(ButtonStyle::Tinted(TintColor::Warning))
                            .tooltip(Tooltip::text("Push with --force-with-lease"))
                            .disabled(!can_force_push)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.push(PushMode::ForceWithLease, window, cx)
                            })),
                    )
                    .child(
                        Button::new("push-confirm", "Push")
                            .style(ButtonStyle::Filled)
                            .disabled(!can_push)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.push(PushMode::Normal, window, cx)
                            })),
                    ),
            )
    }
}

fn placeholder(message: impl Into<SharedString>, color: Color) -> AnyElement {
    div()
        .p_3()
        .child(Label::new(message).size(LabelSize::Small).color(color))
        .into_any_element()
}
