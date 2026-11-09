use db::kvp::KeyValueStore;
use gpui::{App, Task};
use serde::{Deserialize, Serialize};
use util::ResultExt as _;

const KEY: &str = "git_graph_view_options";

/// Hidden flags and width fractions of the table columns. Empty means "never customized".
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct ColumnLayout {
    pub(crate) hidden: Vec<bool>,
    pub(crate) fractions: Vec<f32>,
}

/// Layout choices of the git graph that outlive a single graph view.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct ViewOptions {
    pub(crate) branches_visible: bool,
    /// Ref labels inside the description column instead of a wide gutter left of the graph.
    pub(crate) refs_in_description: bool,
    pub(crate) sidebar_width: Option<f32>,
    pub(crate) log_columns: ColumnLayout,
    /// File history has its own, narrower set of columns.
    pub(crate) path_columns: ColumnLayout,
}

impl Default for ViewOptions {
    fn default() -> Self {
        Self {
            branches_visible: true,
            refs_in_description: true,
            sidebar_width: None,
            log_columns: ColumnLayout::default(),
            path_columns: ColumnLayout::default(),
        }
    }
}

impl ViewOptions {
    pub(crate) fn columns(&self, is_path_history: bool) -> &ColumnLayout {
        if is_path_history {
            &self.path_columns
        } else {
            &self.log_columns
        }
    }

    pub(crate) fn columns_mut(&mut self, is_path_history: bool) -> &mut ColumnLayout {
        if is_path_history {
            &mut self.path_columns
        } else {
            &mut self.log_columns
        }
    }

    pub(crate) fn load(cx: &App) -> Self {
        KeyValueStore::global(cx)
            .read_kvp(KEY)
            .log_err()
            .flatten()
            .and_then(|json| serde_json::from_str(&json).log_err())
            .unwrap_or_default()
    }

    /// Foreground tasks start in spawn order and the database queue is FIFO, so an older
    /// snapshot can not land after a newer one (background tasks would not guarantee that).
    pub(crate) fn save(&self, cx: &App) {
        self.save_task(cx).detach();
    }

    pub(crate) fn save_task(&self, cx: &App) -> Task<()> {
        let Some(json) = serde_json::to_string(self).log_err() else {
            return Task::ready(());
        };
        let store = KeyValueStore::global(cx);
        cx.spawn(async move |_| {
            store.write_kvp(KEY.into(), json).await.log_err();
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_saved_before_the_layout_existed_still_load() {
        let options: ViewOptions =
            serde_json::from_str(r#"{"branches_visible":false,"refs_in_description":true}"#)
                .unwrap();
        assert!(!options.branches_visible);
        assert_eq!(options.sidebar_width, None);
        assert_eq!(options.log_columns, ColumnLayout::default());
    }

    #[test]
    fn layout_survives_json_per_kind_of_graph() {
        let mut options = ViewOptions::default();
        options.sidebar_width = Some(300.0);
        *options.columns_mut(false) = ColumnLayout {
            hidden: vec![false, true, false, false, false],
            fractions: vec![0.3, 0.37, 0.13, 0.11, 0.09],
        };
        let restored: ViewOptions =
            serde_json::from_str(&serde_json::to_string(&options).unwrap()).unwrap();
        assert_eq!(restored, options);
        assert_eq!(restored.columns(true), &ColumnLayout::default());
    }
}
