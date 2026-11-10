use crate::git_graph::GitGraph;
use db::kvp::KeyValueStore;
use git::repository::LogSource;
use gpui::{
    App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable, Pixels,
    Subscription, WeakEntity, Window, actions, px,
};
use project::git_store::{GitStore, GitStoreEvent, RepositoryId};
use theme::ActiveTheme as _;
use ui::{IconButton, IconButtonShape, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    ToggleZoom, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

actions!(
    git_graph,
    [
        /// Toggles focus on the git graph panel.
        ToggleFocus,
        /// Toggles the git graph panel, closing it when it loses focus.
        Toggle
    ]
);

const POSITION_KEY: &str = "git_graph_panel_position";

fn load_position(cx: &App) -> DockPosition {
    let stored = KeyValueStore::global(cx)
        .read_kvp(POSITION_KEY)
        .log_err()
        .flatten();
    match stored.as_deref() {
        Some("left") => DockPosition::Left,
        Some("right") => DockPosition::Right,
        _ => DockPosition::Bottom,
    }
}

fn save_position(position: DockPosition, cx: &App) {
    let name = match position {
        DockPosition::Left => "left",
        DockPosition::Right => "right",
        DockPosition::Bottom => "bottom",
    };
    let store = KeyValueStore::global(cx);
    cx.spawn(async move |_| {
        store
            .write_kvp(POSITION_KEY.into(), name.into())
            .await
            .log_err();
    })
    .detach();
}

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
        workspace.toggle_panel_focus::<GitGraphPanel>(window, cx);
    });
    workspace.register_action(|workspace, _: &Toggle, window, cx| {
        if !workspace.toggle_panel_focus::<GitGraphPanel>(window, cx) {
            workspace.close_panel::<GitGraphPanel>(window, cx);
        }
    });
}

/// Hosts the git graph in a dock so the log is not squeezed into a center pane tab.
pub struct GitGraphPanel {
    workspace: WeakEntity<Workspace>,
    git_store: Entity<GitStore>,
    graph: Option<Entity<GitGraph>>,
    focus_handle: FocusHandle,
    position: DockPosition,
    zoomed: bool,
    _subscription: Subscription,
}

impl GitGraphPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, _window, cx| {
            let git_store = workspace.project().read(cx).git_store().clone();
            let workspace_handle = workspace.weak_handle();
            cx.new(|cx| GitGraphPanel::new(workspace_handle, git_store, cx))
        })
    }

    fn new(
        workspace: WeakEntity<Workspace>,
        git_store: Entity<GitStore>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.subscribe(&git_store, |this, _, event, cx| {
            if let GitStoreEvent::ActiveRepositoryChanged(Some(repo_id)) = event
                && this
                    .graph
                    .as_ref()
                    .is_some_and(|graph| graph.read(cx).repo_id() != *repo_id)
            {
                this.graph = None;
                cx.notify();
            }
        });

        Self {
            workspace,
            git_store,
            graph: None,
            focus_handle: cx.focus_handle(),
            position: load_position(cx),
            zoomed: false,
            _subscription: subscription,
        }
    }

    pub fn show_graph(
        &mut self,
        repo_id: RepositoryId,
        log_source: LogSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<GitGraph> {
        if let Some(graph) = self.graph.as_ref()
            && graph.read(cx).repo_id() == repo_id
        {
            let current = graph.read(cx).log_source().clone();
            // Column layout differs for file history, so only plain logs can switch in place.
            let switches_in_place = !current.is_path_history() && !log_source.is_path_history();
            if current == log_source {
                return graph.clone();
            }
            if switches_in_place {
                graph.update(cx, |graph, cx| graph.set_log_source(log_source, cx));
                return graph.clone();
            }
        }

        let graph = self.build_graph(repo_id, log_source, window, cx);
        self.graph = Some(graph.clone());
        cx.notify();
        graph
    }

    fn build_graph(
        &self,
        repo_id: RepositoryId,
        log_source: LogSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<GitGraph> {
        let git_store = self.git_store.clone();
        let workspace = self.workspace.clone();
        cx.new(|cx| GitGraph::new(repo_id, git_store, workspace, Some(log_source), window, cx))
    }

    // Called from render, so it must not notify.
    fn ensure_graph(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.graph.is_some() {
            return;
        }
        let Some(repo_id) = self
            .git_store
            .read(cx)
            .active_repository()
            .map(|repository| repository.read(cx).id)
        else {
            return;
        };
        self.graph = Some(self.build_graph(repo_id, LogSource::All, window, cx));
    }

    fn toggle_zoom(&mut self, cx: &mut Context<Self>) {
        cx.emit(if self.zoomed {
            PanelEvent::ZoomOut
        } else {
            PanelEvent::ZoomIn
        });
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let zoomed = self.zoomed;
        let history_path = self.graph.as_ref().and_then(|graph| {
            let graph = graph.read(cx);
            let path = graph.log_source().path()?;
            Some((graph.repo_id(), path.as_unix_str().to_string()))
        });

        h_flex()
            .w_full()
            .flex_none()
            .px_2()
            .py_0p5()
            .gap_1p5()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                h_flex()
                    .min_w_0()
                    .gap_1p5()
                    .child(
                        Icon::new(IconName::GitGraph)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        match &history_path {
                            Some((_, path)) => Label::new(format!("History: {path}")),
                            None => Label::new("Git Graph"),
                        }
                        .size(LabelSize::Small)
                        .truncate(),
                    )
                    .when_some(history_path, |this, (repo_id, _)| {
                        this.child(
                            IconButton::new("git-graph-show-full-log", IconName::Close)
                                .icon_size(IconSize::XSmall)
                                .shape(IconButtonShape::Square)
                                .tooltip(Tooltip::text("Show All Commits"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.show_graph(repo_id, LogSource::All, window, cx);
                                })),
                        )
                    }),
            )
            .child(
                IconButton::new("git-graph-toggle-zoom", IconName::Maximize)
                    .icon_size(IconSize::Small)
                    .toggle_state(zoomed)
                    .selected_icon(IconName::Minimize)
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_zoom(cx)))
                    .tooltip(move |_, cx| {
                        Tooltip::for_action(
                            if zoomed { "Zoom Out" } else { "Zoom In" },
                            &ToggleZoom,
                            cx,
                        )
                    }),
            )
    }
}

impl Focusable for GitGraphPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for GitGraphPanel {}

impl Render for GitGraphPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_graph(window, cx);

        v_flex()
            .key_context("GitGraphPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &ToggleZoom, _, cx| this.toggle_zoom(cx)))
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.render_header(cx))
            .map(|this| match self.graph.as_ref() {
                Some(graph) => this.child(div().flex_1().min_h_0().child(graph.clone())),
                None => this.justify_center().items_center().child(
                    Label::new("No repository found")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            })
    }
}

impl Panel for GitGraphPanel {
    fn persistent_name() -> &'static str {
        "GitGraphPanel"
    }

    fn panel_key() -> &'static str {
        "GitGraphPanel"
    }

    fn activation_focus_handle(&self, cx: &App) -> FocusHandle {
        self.graph
            .as_ref()
            .map_or_else(|| self.focus_handle.clone(), |graph| graph.focus_handle(cx))
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, _: DockPosition) -> bool {
        true
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        self.position = position;
        save_position(position, cx);
        cx.notify();
    }

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        match self.position {
            DockPosition::Left | DockPosition::Right => px(480.),
            DockPosition::Bottom => px(360.),
        }
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::GitGraph)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Git Graph")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    fn is_zoomed(&self, _: &Window, _: &App) -> bool {
        self.zoomed
    }

    fn set_zoomed(&mut self, zoomed: bool, _: &mut Window, cx: &mut Context<Self>) {
        self.zoomed = zoomed;
        cx.notify();
    }

    fn activation_priority(&self) -> u32 {
        4
    }
}
