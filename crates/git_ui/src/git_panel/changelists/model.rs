use collections::{HashMap, HashSet};
use git::repository::RepoPath;
use gpui::SharedString;
use serde::{Deserialize, Serialize};

/// Random, so ids stay unique across repositories and are never reused after a delete.
/// The panel keys collapse and tree state by id and would otherwise mix up repositories.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(in crate::git_panel) struct ChangelistId(u64);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(in crate::git_panel) struct Changelist {
    pub(in crate::git_panel) id: ChangelistId,
    pub(in crate::git_panel) name: SharedString,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(in crate::git_panel) struct Changelists {
    pub(super) lists: Vec<Changelist>,
    pub(super) assignments: HashMap<RepoPath, ChangelistId>,
}

impl Changelists {
    pub(in crate::git_panel) fn lists(&self) -> &[Changelist] {
        &self.lists
    }

    pub(in crate::git_panel) fn name(&self, id: ChangelistId) -> Option<&SharedString> {
        self.lists
            .iter()
            .find(|list| list.id == id)
            .map(|list| &list.name)
    }

    pub(in crate::git_panel) fn list_of(&self, path: &RepoPath) -> Option<ChangelistId> {
        self.assignments.get(path).copied()
    }

    /// Returns the existing list when the name is taken, so creating twice is harmless.
    pub(in crate::git_panel) fn create(&mut self, name: &str) -> Option<ChangelistId> {
        let name = name.trim();
        if name.is_empty() {
            return None;
        }
        if let Some(existing) = self.lists.iter().find(|list| list.name == name) {
            return Some(existing.id);
        }
        let id = ChangelistId(rand::random());
        self.lists.push(Changelist {
            id,
            name: name.to_string().into(),
        });
        Some(id)
    }

    pub(in crate::git_panel) fn rename(&mut self, id: ChangelistId, name: &str) -> bool {
        let name = name.trim();
        if name.is_empty() || self.lists.iter().any(|list| list.name == name) {
            return false;
        }
        match self.lists.iter_mut().find(|list| list.id == id) {
            Some(list) => {
                list.name = name.to_string().into();
                true
            }
            None => false,
        }
    }

    pub(in crate::git_panel) fn delete(&mut self, id: ChangelistId) {
        self.lists.retain(|list| list.id != id);
        self.assignments.retain(|_, assigned| *assigned != id);
    }

    /// `None` takes the paths out of every list.
    pub(in crate::git_panel) fn assign(
        &mut self,
        paths: impl IntoIterator<Item = RepoPath>,
        list: Option<ChangelistId>,
    ) {
        match list {
            Some(id) if self.lists.iter().any(|existing| existing.id == id) => {
                self.assignments
                    .extend(paths.into_iter().map(|path| (path, id)));
            }
            Some(_) => {}
            None => {
                for path in paths {
                    self.assignments.remove(&path);
                }
            }
        }
    }

    pub(super) fn has_stale(&self, changed: &HashSet<RepoPath>) -> bool {
        self.assignments.keys().any(|path| !changed.contains(path))
    }

    /// Drops assignments of files that no longer have changes.
    pub(in crate::git_panel) fn prune(&mut self, changed: &HashSet<RepoPath>) {
        self.assignments.retain(|path, _| changed.contains(path));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git::repository::repo_path;

    #[test]
    fn create_trims_and_reuses_existing_names() {
        let mut lists = Changelists::default();
        assert_eq!(lists.create("   "), None);
        let first = lists.create("  Local ").unwrap();
        assert_eq!(lists.create("Local"), Some(first));
        assert_eq!(lists.lists().len(), 1);
        assert_eq!(lists.name(first).map(|name| name.as_ref()), Some("Local"));
    }

    #[test]
    fn rename_refuses_taken_and_empty_names() {
        let mut lists = Changelists::default();
        let local = lists.create("Local").unwrap();
        let feature = lists.create("Feature").unwrap();
        assert!(!lists.rename(feature, "Local"));
        assert!(!lists.rename(feature, " "));
        assert!(lists.rename(local, "Machine"));
        assert_eq!(lists.name(local).map(|name| name.as_ref()), Some("Machine"));
    }

    #[test]
    fn delete_releases_files_and_ids_are_not_reused() {
        let mut lists = Changelists::default();
        let local = lists.create("Local").unwrap();
        lists.assign([repo_path("a.rs"), repo_path("b.rs")], Some(local));
        lists.delete(local);
        assert_eq!(lists.list_of(&repo_path("a.rs")), None);

        let again = lists.create("Local").unwrap();
        assert_ne!(again, local);
        assert_eq!(lists.list_of(&repo_path("b.rs")), None);
    }

    #[test]
    fn assign_moves_between_lists_and_back_to_none() {
        let mut lists = Changelists::default();
        let one = lists.create("One").unwrap();
        let two = lists.create("Two").unwrap();
        let path = repo_path("a.rs");

        lists.assign([path.clone()], Some(one));
        lists.assign([path.clone()], Some(two));
        assert_eq!(lists.list_of(&path), Some(two));

        lists.assign([path.clone()], None);
        assert_eq!(lists.list_of(&path), None);
    }

    #[test]
    fn assigning_to_a_missing_list_does_nothing() {
        let mut lists = Changelists::default();
        let gone = lists.create("Gone").unwrap();
        lists.delete(gone);
        lists.assign([repo_path("a.rs")], Some(gone));
        assert_eq!(lists.list_of(&repo_path("a.rs")), None);
    }

    #[test]
    fn prune_keeps_only_changed_files() {
        let mut lists = Changelists::default();
        let local = lists.create("Local").unwrap();
        lists.assign([repo_path("kept.rs"), repo_path("gone.rs")], Some(local));

        let changed = HashSet::from_iter([repo_path("kept.rs")]);
        assert!(lists.has_stale(&changed));
        lists.prune(&changed);
        assert_eq!(lists.list_of(&repo_path("kept.rs")), Some(local));
        assert_eq!(lists.list_of(&repo_path("gone.rs")), None);
        assert!(!lists.has_stale(&changed));
    }
}
