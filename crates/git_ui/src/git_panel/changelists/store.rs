//! One in-memory document per repository, shared by every panel in the process.
//! The database is only read once per repository and written after each change, so a
//! read right after a write can never see stale data and panels in other windows stay in sync.

use super::model::{Changelist, Changelists};
use collections::HashMap;
use db::kvp::KeyValueStore;
use git::repository::RepoPath;
use gpui::{App, BorrowAppContext as _, Global};
use serde::{Deserialize, Serialize};
use std::{cell::RefCell, collections::BTreeMap, path::Path};
use util::ResultExt as _;

#[derive(Default)]
pub(in crate::git_panel) struct Documents(RefCell<HashMap<String, Changelists>>);

impl Global for Documents {}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Stored {
    lists: Vec<Changelist>,
    assignments: BTreeMap<String, super::ChangelistId>,
}

impl From<&Changelists> for Stored {
    fn from(lists: &Changelists) -> Self {
        Self {
            lists: lists.lists.clone(),
            assignments: lists
                .assignments
                .iter()
                .map(|(path, id)| (path.as_unix_str().to_string(), *id))
                .collect(),
        }
    }
}

impl From<Stored> for Changelists {
    fn from(stored: Stored) -> Self {
        Self {
            lists: stored.lists,
            assignments: stored
                .assignments
                .into_iter()
                .filter_map(|(path, id)| Some((RepoPath::from_proto(&path).log_err()?, id)))
                .collect(),
        }
    }
}

pub(super) fn init(cx: &mut App) {
    if !cx.has_global::<Documents>() {
        cx.set_global(Documents::default());
    }
}

pub(super) fn key(work_directory: &Path) -> String {
    format!("git_changelists:{}", work_directory.display())
}

pub(super) fn get(key: &str, cx: &App) -> Changelists {
    cx.global::<Documents>()
        .0
        .borrow_mut()
        .entry(key.to_string())
        .or_insert_with(|| load(key, cx))
        .clone()
}

/// Applies `change`, persists the result and notifies observers of [`Documents`].
pub(super) fn update<R>(
    key: &str,
    cx: &mut App,
    change: impl FnOnce(&mut Changelists) -> R,
) -> (Changelists, R) {
    cx.update_global::<Documents, _>(|documents, cx| {
        let mut documents = documents.0.borrow_mut();
        let document = documents
            .entry(key.to_string())
            .or_insert_with(|| load(key, cx));
        let result = change(document);
        save(key, document, cx);
        (document.clone(), result)
    })
}

fn load(key: &str, cx: &App) -> Changelists {
    KeyValueStore::global(cx)
        .read_kvp(key)
        .log_err()
        .flatten()
        .and_then(|json| serde_json::from_str::<Stored>(&json).log_err())
        .unwrap_or_default()
        .into()
}

/// Foreground tasks start in spawn order and the database queue is FIFO, so an older
/// snapshot can not land after a newer one (background tasks would not guarantee that).
fn save(key: &str, lists: &Changelists, cx: &App) {
    let Some(json) = serde_json::to_string(&Stored::from(lists)).log_err() else {
        return;
    };
    let store = KeyValueStore::global(cx);
    let key = key.to_string();
    cx.spawn(async move |_| {
        store.write_kvp(key, json).await.log_err();
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use git::repository::repo_path;

    #[test]
    fn lists_and_assignments_survive_json() {
        let mut lists = Changelists::default();
        let local = lists.create("Local").unwrap();
        lists.assign([repo_path("src/a.rs")], Some(local));

        let json = serde_json::to_string(&Stored::from(&lists)).unwrap();
        let restored: Changelists = serde_json::from_str::<Stored>(&json).unwrap().into();

        assert_eq!(restored, lists);
    }

    #[test]
    fn unknown_or_missing_fields_fall_back_to_empty() {
        let restored: Changelists = serde_json::from_str::<Stored>("{}").unwrap().into();
        assert_eq!(restored, Changelists::default());
    }

    #[test]
    fn key_is_per_repository() {
        assert_ne!(key(Path::new("/a")), key(Path::new("/b")));
    }
}
