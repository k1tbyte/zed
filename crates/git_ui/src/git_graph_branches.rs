mod cleanup;
mod menu;
mod operations;
mod tree;

pub(crate) use self::operations::new_branch_from_ref;

use self::menu::{branch_context_menu, cleanup_menu};
use self::tree::{Row, build_rows, refs_between};
use crate::git_job::{JobContext, RunningJobs};
use editor::{Editor, EditorEvent};
use git::repository::{Branch, LogSource, UpstreamTrackingStatus};
use gpui::{
    AnyElement, App, ClickEvent, Context, DragMoveEvent, Entity, EventEmitter,
    ListHorizontalSizingBehavior, SharedString, UniformListScrollHandle, WeakEntity, Window,
    relative, uniform_list,
};
use project::git_store::Repository;
use project::git_store::{GitStore, GitStoreEvent, RepositoryEvent, RepositoryId};
use std::{collections::HashSet, ops::Range, rc::Rc, sync::Arc};
use theme::ActiveTheme as _;
use ui::{
    CommonAnimationExt as _, IndentGuideColors, ListItem, ListItemSpacing, ScrollAxes, Scrollbars,
    Tooltip, WithScrollbar, indent_guides, prelude::*, right_click_menu,
};
use workspace::Workspace;

pub(crate) const DEFAULT_WIDTH: Pixels = px(260.);
const MIN_WIDTH: Pixels = px(160.);
const MAX_WIDTH: Pixels = px(640.);
const RESIZE_HANDLE_WIDTH: Pixels = px(6.);
const TREE_INDENT: Pixels = px(12.);
// Row padding plus half a chevron, so a guide runs under the parent's chevron.
const INDENT_GUIDE_LEFT_OFFSET: Pixels = px(11.);

pub(crate) enum BranchesSidebarEvent {
    SelectSource(LogSource),
}

pub(crate) struct BranchesSidebar {
    repo_id: RepositoryId,
    git_store: Entity<GitStore>,
    workspace: WeakEntity<Workspace>,
    filter_editor: Entity<Editor>,
    collapsed: HashSet<SharedString>,
    active_source: LogSource,
    scroll_handle: UniformListScrollHandle,
    width: Pixels,
    selected: HashSet<SharedString>,
    anchor: Option<SharedString>,
    running: Entity<RunningJobs>,
    cached_rows: Option<CachedRows>,
}

/// The tree built from one branch list. Building it sorts and clones every branch, which is
/// too much for each frame of the operation spinner.
struct CachedRows {
    branches: Arc<[Branch]>,
    rows: Rc<Vec<Row>>,
    widest_row: Option<usize>,
}

struct DraggedBranchesResize;

impl EventEmitter<BranchesSidebarEvent> for BranchesSidebar {}
impl BranchesSidebar {
    pub(crate) fn new(
        repo_id: RepositoryId,
        git_store: Entity<GitStore>,
        workspace: WeakEntity<Workspace>,
        active_source: LogSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Filter branches…", window, cx);
            editor
        });
        cx.subscribe(&filter_editor, |this, _, event: &EditorEvent, cx| {
            if matches!(event, EditorEvent::BufferEdited) {
                this.cached_rows = None;
                cx.notify();
            }
        })
        .detach();
        cx.subscribe(&git_store, |this, _, event, cx| {
            if let GitStoreEvent::RepositoryUpdated(
                repo_id,
                RepositoryEvent::HeadChanged | RepositoryEvent::BranchListChanged,
                _,
            ) = event
                && *repo_id == this.repo_id
            {
                cx.notify();
            }
        })
        .detach();
        let running = cx.new(|_| RunningJobs::default());
        cx.observe(&running, |_, _, cx| cx.notify()).detach();

        Self {
            repo_id,
            git_store,
            workspace,
            filter_editor,
            collapsed: HashSet::new(),
            active_source,
            scroll_handle: UniformListScrollHandle::new(),
            width: DEFAULT_WIDTH,
            selected: HashSet::new(),
            anchor: None,
            running,
            cached_rows: None,
        }
    }

    fn job_context(&self, repository: Entity<Repository>) -> JobContext {
        JobContext {
            workspace: self.workspace.clone(),
            repository,
            running: Some(self.running.clone()),
        }
    }

    pub(crate) fn width(&self) -> Pixels {
        self.width
    }

    pub(crate) fn set_width(&mut self, width: Pixels, cx: &mut Context<Self>) {
        self.width = width.clamp(MIN_WIDTH, MAX_WIDTH);
        cx.notify();
    }

    pub(crate) fn set_active_source(&mut self, source: LogSource, cx: &mut Context<Self>) {
        self.active_source = source;
        cx.notify();
    }

    fn repository(&self, cx: &App) -> Option<Entity<Repository>> {
        self.git_store
            .read(cx)
            .repositories()
            .get(&self.repo_id)
            .cloned()
    }

    // Update, Rebase and Push shell out to git on this machine, so they need a local project.
    fn is_local(&self, cx: &App) -> bool {
        self.git_store.read(cx).is_local()
    }

    // Selecting the active source again returns to the unfiltered log.
    fn select(&mut self, source: LogSource, cx: &mut Context<Self>) {
        let source = if self.active_source == source {
            LogSource::All
        } else {
            source
        };
        self.selected = match &source {
            LogSource::Branch(ref_name) if ref_name.as_str() != "HEAD" => {
                HashSet::from([ref_name.clone()])
            }
            _ => HashSet::new(),
        };
        self.anchor = self.selected.iter().next().cloned();
        cx.emit(BranchesSidebarEvent::SelectSource(source));
    }

    fn toggle_selected(&mut self, branch: &Branch, cx: &mut Context<Self>) {
        if !self.selected.remove(&branch.ref_name) {
            self.selected.insert(branch.ref_name.clone());
        }
        self.anchor = Some(branch.ref_name.clone());
        cx.notify();
    }

    fn select_range_to(&mut self, branch: &Branch, cx: &mut Context<Self>) {
        let (rows, _) = self.rows(cx);
        if let Some(selected) = refs_between(&rows, self.anchor.as_ref(), &branch.ref_name) {
            self.selected = selected;
            cx.notify();
        }
    }

    /// The rows and the index of the widest one. A new branch list is noticed here, a changed
    /// filter or folder state drops the cache where it changes.
    fn rows(&mut self, cx: &App) -> (Rc<Vec<Row>>, Option<usize>) {
        let branches = self
            .repository(cx)
            .map(|repository| repository.read(cx).branch_list.clone())
            .unwrap_or_default();
        let cached = self
            .cached_rows
            .take()
            .filter(|cached| Arc::ptr_eq(&cached.branches, &branches))
            .unwrap_or_else(|| {
                let filter = self.filter_editor.read(cx).text(cx);
                let rows = build_rows(&branches, &filter, &self.collapsed);
                let widest_row = rows
                    .iter()
                    .enumerate()
                    .max_by_key(|(_, row)| row.width_hint())
                    .map(|(ix, _)| ix);
                CachedRows {
                    branches,
                    rows: Rc::new(rows),
                    widest_row,
                }
            });
        let rows = (cached.rows.clone(), cached.widest_row);
        self.cached_rows = Some(cached);
        rows
    }

    fn selected_branches(&self, cx: &App) -> Vec<Branch> {
        let Some(repository) = self.repository(cx) else {
            return Vec::new();
        };
        repository
            .read(cx)
            .branch_list
            .iter()
            .filter(|branch| self.selected.contains(&branch.ref_name))
            .cloned()
            .collect()
    }

    fn toggle_folder(&mut self, key: &SharedString, cx: &mut Context<Self>) {
        if !self.collapsed.remove(key) {
            self.collapsed.insert(key.clone());
        }
        self.cached_rows = None;
        cx.notify();
    }

    fn render_row(&self, ix: usize, row: &Row, cx: &mut Context<Self>) -> AnyElement {
        match row {
            Row::Head => {
                let source = LogSource::Branch("HEAD".into());
                ListItem::new(("branch-row", ix))
                    .spacing(ListItemSpacing::Sparse)
                    .toggle_state(self.active_source == source)
                    .start_slot(
                        Icon::new(IconName::GitBranch)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new("HEAD (Current Branch)").size(LabelSize::Small))
                    .on_click(cx.listener(move |this, _, _, cx| this.select(source.clone(), cx)))
                    .into_any_element()
            }
            Row::Folder {
                key,
                name,
                depth,
                expanded,
            } => {
                let icon = if *expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                };
                let key = key.clone();
                ListItem::new(("branch-folder", ix))
                    .spacing(ListItemSpacing::Sparse)
                    .indent_level(*depth)
                    .indent_step_size(TREE_INDENT)
                    .start_slot(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted))
                    .child(Label::new(name.clone()).size(LabelSize::Small))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_folder(&key, cx)))
                    .into_any_element()
            }
            Row::Branch {
                branch,
                label,
                depth,
            } => self.render_branch_row(ix, branch, label, *depth, cx),
        }
    }

    fn render_branch_row(
        &self,
        ix: usize,
        branch: &Branch,
        label: &SharedString,
        depth: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (icon, color) = if branch.is_head {
            (IconName::StarFilled, Color::Warning)
        } else {
            (IconName::GitBranch, Color::Muted)
        };
        let tracking = branch.tracking_status();
        let tooltip = match (&branch.upstream, tracking) {
            (Some(upstream), Some(status)) => format!(
                "{}\n{} ahead, {} behind {}",
                branch.name(),
                status.ahead,
                status.behind,
                upstream.stripped_ref_name().unwrap_or_default()
            ),
            _ => branch.name().to_string(),
        };
        let is_selected = self.selected.contains(&branch.ref_name)
            || self.active_source == LogSource::Branch(branch.ref_name.clone());
        let item = ListItem::new(("branch-row", ix))
            .spacing(ListItemSpacing::Sparse)
            .indent_level(depth)
            .indent_step_size(TREE_INDENT)
            .toggle_state(is_selected)
            .start_slot(Icon::new(icon).size(IconSize::Small).color(color))
            .child(Label::new(label.clone()).size(LabelSize::Small))
            .end_slot::<AnyElement>(
                tracking
                    .filter(|status| status.ahead > 0 || status.behind > 0)
                    .map(tracking_badges),
            )
            .tooltip(Tooltip::text(tooltip))
            .on_click(cx.listener({
                let branch = branch.clone();
                move |this, event: &ClickEvent, window, cx| {
                    let modifiers = event.modifiers();
                    if event.click_count() == 2 && !branch.is_remote() {
                        this.checkout(&branch, window, cx);
                    } else if modifiers.shift {
                        this.select_range_to(&branch, cx);
                    } else if modifiers.secondary(cx) {
                        this.toggle_selected(&branch, cx);
                    } else {
                        this.select(LogSource::Branch(branch.ref_name.clone()), cx);
                    }
                }
            }));

        let sidebar = cx.entity().downgrade();
        let branch = branch.clone();
        right_click_menu(("branch-menu", ix))
            .trigger(move |_, _, _| item)
            .menu(move |window, cx| branch_context_menu(&sidebar, &branch, window, cx))
            .into_any_element()
    }
}

impl Render for BranchesSidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (rows, widest_row) = self.rows(cx);
        let guide_rows = rows.clone();
        let border_color = cx.theme().colors().border;

        v_flex()
            .relative()
            .h_full()
            .w(self.width)
            // Keeps the toolbar's Branches button reachable in a narrow dock.
            .max_w(relative(0.5))
            .flex_none()
            .border_r_1()
            .border_color(border_color)
            .on_drag_move(cx.listener(
                |this, event: &DragMoveEvent<DraggedBranchesResize>, _, cx| {
                    this.width =
                        (event.event.position.x - event.bounds.left()).clamp(MIN_WIDTH, MAX_WIDTH);
                    cx.notify();
                },
            ))
            .child(
                div()
                    .id("git-graph-branches-resize-handle")
                    .absolute()
                    .top_0()
                    .right_0()
                    .h_full()
                    .w(RESIZE_HANDLE_WIDTH)
                    .cursor_col_resize()
                    .block_mouse_except_scroll()
                    .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
                        if event.click_count() >= 2 {
                            this.width = DEFAULT_WIDTH;
                            cx.notify();
                        }
                        cx.stop_propagation();
                    }))
                    .on_drag(DraggedBranchesResize, |_, _, _, cx| cx.new(|_| gpui::Empty)),
            )
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_1p5()
                    .border_b_1()
                    .border_color(border_color)
                    .child(
                        Icon::new(IconName::MagnifyingGlass)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1().child(self.filter_editor.clone()))
                    .when(self.is_local(cx), |this| {
                        this.child(cleanup_menu(cx.weak_entity()))
                    }),
            )
            .child(
                div()
                    .id("git-graph-branches-list")
                    .flex_1()
                    .min_h_0()
                    .child(
                        uniform_list(
                            "git-graph-branches",
                            rows.len(),
                            cx.processor(move |this, range: Range<usize>, _window, cx| {
                                range
                                    .filter_map(|ix| {
                                        rows.get(ix).map(|row| this.render_row(ix, row, cx))
                                    })
                                    .collect()
                            }),
                        )
                        .with_decoration(
                            indent_guides(TREE_INDENT, IndentGuideColors::panel(cx))
                                .with_left_offset(INDENT_GUIDE_LEFT_OFFSET)
                                .with_compute_indents_fn(cx.entity(), move |_, range, _, _| {
                                    range
                                        .map(|ix| guide_rows.get(ix).map_or(0, Row::depth))
                                        .collect()
                                }),
                        )
                        .with_horizontal_sizing_behavior(
                            ListHorizontalSizingBehavior::Unconstrained,
                        )
                        .with_width_from_item(widest_row)
                        .size_full()
                        .track_scroll(&self.scroll_handle),
                    )
                    .custom_scrollbars(
                        Scrollbars::new(ScrollAxes::Both)
                            .tracked_scroll_handle(&self.scroll_handle),
                        window,
                        cx,
                    ),
            )
            .when_some(self.running.read(cx).label(), |this, label| {
                this.child(
                    h_flex()
                        .px_2()
                        .py_1()
                        .gap_1p5()
                        .border_t_1()
                        .border_color(border_color)
                        .child(
                            Icon::new(IconName::LoadCircle)
                                .size(IconSize::XSmall)
                                .color(Color::Accent)
                                .with_rotate_animation(3),
                        )
                        .child(
                            Label::new(label)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                )
            })
    }
}

fn tracking_badges(status: UpstreamTrackingStatus) -> AnyElement {
    let badge = |icon: IconName, count: u32, color: Color| {
        h_flex()
            .gap_0p5()
            .child(Icon::new(icon).size(IconSize::XSmall).color(color))
            .child(
                Label::new(count.to_string())
                    .size(LabelSize::XSmall)
                    .color(color),
            )
    };
    h_flex()
        .gap_1()
        .when(status.behind > 0, |this| {
            this.child(badge(IconName::ArrowDown, status.behind, Color::Accent))
        })
        .when(status.ahead > 0, |this| {
            this.child(badge(IconName::ArrowUp, status.ahead, Color::Created))
        })
        .into_any_element()
}
