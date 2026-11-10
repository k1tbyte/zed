//! Keeps column visibility, column widths and the branches panel width across graph views.
//! Stored in [`ViewOptions`], one layout for the log and one for file history.

use super::GitGraph;
use crate::git_graph_view_options::{ColumnLayout, ViewOptions};
use gpui::{App, Context, DefiniteLength, Task, px};
use std::time::Duration;
use ui::table_row::TableRow;

/// Resizing notifies on every mouse move, so wait for the drag to settle.
const SAVE_DELAY: Duration = Duration::from_millis(500);

/// A column layout and the branches panel width.
type Layout = (ColumnLayout, f32);

pub(super) struct LayoutSync {
    /// What this graph last applied or wrote. Reloading the shared options must not replace it,
    /// or an untouched graph would write back its own old layout over another graph's.
    baseline: Option<Layout>,
    save: Task<()>,
}

impl LayoutSync {
    pub(super) fn new() -> Self {
        Self {
            baseline: None,
            save: Task::ready(()),
        }
    }
}

impl GitGraph {
    fn is_path_history(&self) -> bool {
        self.log_source.is_path_history()
    }

    fn current_layout(&self, cx: &App) -> Option<Layout> {
        let fractions = self
            .column_widths
            .read(cx)
            .preview_widths()
            .as_slice()
            .iter()
            .map(|width| match width {
                DefiniteLength::Fraction(fraction) => Some(*fraction),
                DefiniteLength::Absolute(_) => None,
            })
            .collect::<Option<Vec<_>>>()?;
        let layout = ColumnLayout {
            hidden: self.column_visibility.as_slice().to_vec(),
            fractions,
        };
        Some((layout, f32::from(self.branches_sidebar.read(cx).width())))
    }

    /// Applies the stored layout and starts watching for changes to it.
    pub(super) fn restore_layout(&mut self, cx: &mut Context<Self>) {
        let layout = self.view_options.columns(self.is_path_history()).clone();
        let columns = self.column_visibility.cols();
        if layout.hidden.len() == columns {
            self.column_visibility = TableRow::from_vec(layout.hidden, columns);
        }
        self.column_widths
            .update(cx, |state, _| state.restore_fractions(&layout.fractions));
        if let Some(width) = self.view_options.sidebar_width {
            self.branches_sidebar
                .update(cx, |sidebar, cx| sidebar.set_width(px(width), cx));
        }
        self.layout.baseline = self.current_layout(cx);

        cx.observe(&self.column_widths, |this, _, cx| this.save_layout_soon(cx))
            .detach();
        cx.observe(&self.branches_sidebar, |this, _, cx| {
            this.save_layout_soon(cx)
        })
        .detach();
        // A change made just before the graph goes away would otherwise die with the debounce.
        cx.on_release(|this, cx| this.flush_layout(cx)).detach();
    }

    pub(super) fn save_layout_soon(&mut self, cx: &mut Context<Self>) {
        self.layout.save = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            this.update(cx, |this, cx| this.flush_layout(cx)).ok();
        });
    }

    /// Writes the layout when it differs from what this graph last applied or wrote.
    fn flush_layout(&mut self, cx: &mut App) {
        let Some(current) = self.current_layout(cx) else {
            return;
        };
        if self.layout.baseline.as_ref() == Some(&current) {
            return;
        }
        let is_path_history = self.is_path_history();
        // Starts from what is stored, so another graph's change to the other options survives.
        let mut options = ViewOptions::load(cx);
        *options.columns_mut(is_path_history) = current.0.clone();
        options.sidebar_width = Some(current.1);
        options.save(cx);
        self.layout.baseline = Some(current);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use db::kvp::KeyValueStore;
    use fs::FakeFs;
    use gpui::{Entity, TestAppContext, VisualContext as _, VisualTestContext};
    use project::Project;
    use serde_json::json;
    use settings::SettingsStore;
    use std::path::Path;
    use workspace::MultiWorkspace;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            language_model::init(cx);
            crate::init(cx);
            // The shared fallback database would leak layouts between tests.
            cx.set_global(db::AppDatabase::test_new());
        });
    }

    async fn open_graph(cx: &mut TestAppContext) -> (Entity<GitGraph>, VisualTestContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(Path::new("/project"), json!({ ".git": {}, "a.txt": "a" }))
            .await;
        let project = Project::test(fs, [Path::new("/project")], cx).await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        cx.run_until_parked();
        let repository_id = project.read_with(cx, |project, cx| {
            project.active_repository(cx).unwrap().read(cx).id
        });
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| {
                multi_workspace.workspace().downgrade()
            })
            .unwrap();
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository_id,
                project.read(cx).git_store().clone(),
                workspace,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        (graph, cx)
    }

    /// The options as stored, after every write queued so far has landed (the queue is FIFO).
    async fn stored_options(cx: &mut VisualTestContext) -> ViewOptions {
        // Runs pending effects such as entity releases, then lets the writes they queued start.
        cx.update(|_, _| ());
        cx.run_until_parked();
        let store = cx.update(|_, cx| KeyValueStore::global(cx));
        store
            .write_kvp("barrier".into(), String::new())
            .await
            .unwrap();
        cx.update(|_, cx| ViewOptions::load(cx))
    }

    fn settle(cx: &mut VisualTestContext) {
        cx.executor().advance_clock(SAVE_DELAY * 2);
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn a_new_graph_starts_with_the_saved_layout(cx: &mut TestAppContext) {
        init_test(cx);
        let mut saved = ViewOptions::default();
        *saved.columns_mut(false) = ColumnLayout {
            hidden: vec![false, true, false, false, false],
            fractions: vec![0.2, 0.4, 0.15, 0.15, 0.1],
        };
        saved.sidebar_width = Some(321.0);
        cx.update(|cx| saved.save_task(cx)).await;

        let (graph, cx) = open_graph(cx).await;
        let (layout, sidebar_width) =
            graph.read_with(&cx, |graph, cx| graph.current_layout(cx).unwrap());

        assert_eq!(&layout, saved.columns(false));
        assert_eq!(sidebar_width, 321.0);
    }

    #[gpui::test]
    async fn resizing_and_hiding_are_saved_once_they_settle(cx: &mut TestAppContext) {
        init_test(cx);
        let (graph, mut cx) = open_graph(cx).await;
        graph.update(&mut cx, |graph, cx| {
            graph.toggle_column_visibility(1, cx);
            graph.column_widths.update(cx, |state, cx| {
                state.restore_fractions(&[0.1, 0.5, 0.1, 0.2, 0.1]);
                cx.notify();
            });
        });
        settle(&mut cx);

        let stored = stored_options(&mut cx).await;
        assert_eq!(
            stored.columns(false).hidden,
            [false, true, false, false, false]
        );
        assert_eq!(stored.columns(false).fractions, [0.1, 0.5, 0.1, 0.2, 0.1]);
    }

    #[gpui::test]
    async fn a_graph_that_changed_nothing_writes_nothing(cx: &mut TestAppContext) {
        init_test(cx);
        let (graph, mut cx) = open_graph(cx).await;
        graph.update(&mut cx, |graph, cx| {
            graph.column_widths.update(cx, |_, cx| cx.notify());
            graph.branches_sidebar.update(cx, |_, cx| cx.notify());
        });
        settle(&mut cx);

        assert_eq!(stored_options(&mut cx).await, ViewOptions::default());
    }

    #[gpui::test]
    async fn an_untouched_graph_does_not_overwrite_another_graphs_layout(cx: &mut TestAppContext) {
        init_test(cx);
        let (graph, mut cx) = open_graph(cx).await;
        let other = ViewOptions {
            sidebar_width: Some(480.0),
            ..ViewOptions::default()
        };
        cx.update(|_, cx| other.save_task(cx)).await;
        // Any update reloads the shared options into this graph.
        graph.update(&mut cx, |graph, cx| {
            graph.update_view_options(|options| options.refs_in_description = false, cx);
            graph.branches_sidebar.update(cx, |_, cx| cx.notify());
        });
        settle(&mut cx);

        assert_eq!(stored_options(&mut cx).await.sidebar_width, Some(480.0));
    }

    #[gpui::test]
    async fn a_change_made_just_before_the_graph_closes_is_still_saved(cx: &mut TestAppContext) {
        init_test(cx);
        let (graph, mut cx) = open_graph(cx).await;
        graph.update(&mut cx, |graph, cx| graph.toggle_column_visibility(2, cx));
        drop(graph);

        let stored = stored_options(&mut cx).await;
        assert_eq!(
            stored.columns(false).hidden,
            [false, false, true, false, false]
        );
    }
}
