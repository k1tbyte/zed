//! Changelists: named groups of changed files, shown as sections of the Git panel.
//! The panel only calls into this module from a few one-line hooks.
//! Local projects only: the stored key is a path, which is not unique across remote hosts.

mod interaction;
mod model;
mod store;

use super::{GitPanel, GitStatusEntry, Section};
use collections::{HashMap, HashSet};
use git::repository::RepoPath;
use gpui::{App, Context, Entity, Subscription, Window};
use project::git_store::Repository;

pub(in crate::git_panel) use interaction::NewChangelist;
pub(in crate::git_panel) use model::{ChangelistId, Changelists};

#[derive(Clone, Copy, Default)]
struct Tally {
    staged: usize,
    total: usize,
}

impl Tally {
    fn add(&mut self, staged: bool) {
        self.total += 1;
        self.staged += usize::from(staged);
    }
}

/// The panel's view of the shared lists, plus what it derived from them during its last rebuild.
pub(in crate::git_panel) struct ChangelistState {
    key: Option<String>,
    enabled: bool,
    model: Changelists,
    /// Files currently shown inside a list. Empty unless the panel groups by status.
    effective: HashMap<RepoPath, ChangelistId>,
    by_list: HashMap<ChangelistId, Tally>,
    moved_from_tracked: Tally,
    moved_from_new: Tally,
    _documents_changed: Subscription,
}

impl ChangelistState {
    pub(in crate::git_panel) fn new(window: &mut Window, cx: &mut Context<GitPanel>) -> Self {
        store::init(cx);
        Self {
            key: None,
            enabled: false,
            model: Changelists::default(),
            effective: HashMap::default(),
            by_list: HashMap::default(),
            moved_from_tracked: Tally::default(),
            moved_from_new: Tally::default(),
            _documents_changed: cx
                .observe_global_in::<store::Documents>(window, |panel, window, cx| {
                    panel.schedule_update(window, cx)
                }),
        }
    }

    pub(in crate::git_panel) fn model(&self) -> &Changelists {
        &self.model
    }

    /// Lists only exist while the panel groups by status.
    pub(in crate::git_panel) fn is_active(&self) -> bool {
        self.enabled
    }

    pub(in crate::git_panel) fn is_in_list(&self, path: &RepoPath) -> bool {
        self.effective.contains_key(path)
    }

    pub(in crate::git_panel) fn is_in(&self, path: &RepoPath, id: ChangelistId) -> bool {
        self.effective.get(path) == Some(&id)
    }

    pub(in crate::git_panel) fn section_of(&self, path: &RepoPath) -> Option<Section> {
        self.effective.get(path).copied().map(Section::Changelist)
    }

    /// Refreshes the shared lists for `repository` and forgets files that no longer have changes.
    /// Pruning needs the complete status, so it must not wait for the panel's filtered sections.
    pub(in crate::git_panel) fn reconcile(
        &mut self,
        repository: Option<&Entity<Repository>>,
        cx: &mut App,
    ) {
        self.key = repository.and_then(|repository| {
            let repository = repository.read(cx);
            let is_local = repository.git_store()?.read(cx).is_local();
            is_local.then(|| store::key(&repository.work_directory_abs_path))
        });
        let (Some(key), Some(repository)) = (self.key.clone(), repository) else {
            self.model = Changelists::default();
            return;
        };

        self.model = store::get(&key, cx);
        if self.model.assignments.is_empty() {
            return;
        }
        let changed: HashSet<RepoPath> = repository
            .read(cx)
            .cached_status()
            .map(|entry| entry.repo_path)
            .collect();
        // An empty status is indistinguishable from "not scanned yet", so it never prunes.
        if !changed.is_empty() && self.model.has_stale(&changed) {
            self.model = store::update(&key, cx, |model| model.prune(&changed)).0;
        }
    }

    /// Takes the files that belong to a list out of `tracked` and `new` and returns one
    /// section per list, empty ones included so they stay visible as drop targets.
    pub(in crate::git_panel) fn partition(
        &mut self,
        tracked: &mut Vec<GitStatusEntry>,
        new: &mut Vec<GitStatusEntry>,
        by_status: bool,
    ) -> Vec<(Section, Vec<GitStatusEntry>)> {
        self.effective.clear();
        self.enabled = by_status && self.key.is_some();
        if !self.enabled {
            return Vec::new();
        }

        let mut sections: Vec<_> = self
            .model
            .lists()
            .iter()
            .map(|list| (list.id, Vec::new()))
            .collect();
        for entries in [&mut *tracked, &mut *new] {
            for entry in std::mem::take(entries) {
                let bucket = self.model.list_of(&entry.repo_path).and_then(|id| {
                    sections
                        .iter_mut()
                        .find(|(list, _)| *list == id)
                        .map(|(_, bucket)| (id, bucket))
                });
                match bucket {
                    Some((id, bucket)) => {
                        self.effective.insert(entry.repo_path.clone(), id);
                        bucket.push(entry);
                    }
                    None => entries.push(entry),
                }
            }
        }
        sections
            .into_iter()
            .map(|(id, entries)| (Section::Changelist(id), entries))
            .collect()
    }

    pub(in crate::git_panel) fn reset_tallies(&mut self) {
        self.by_list.clear();
        self.moved_from_tracked = Tally::default();
        self.moved_from_new = Tally::default();
    }

    /// Call for every non-conflicted file while the panel recounts.
    pub(in crate::git_panel) fn tally(&mut self, entry: &GitStatusEntry, staged: bool) {
        let Some(id) = self.effective.get(&entry.repo_path) else {
            return;
        };
        self.by_list.entry(*id).or_default().add(staged);
        if entry.status.is_created() {
            self.moved_from_new.add(staged);
        } else {
            self.moved_from_tracked.add(staged);
        }
    }

    /// `(staged, total)` of what a header actually shows, given the panel's totals for its kind.
    pub(in crate::git_panel) fn adjust(
        &self,
        section: Section,
        (staged, total): (usize, usize),
    ) -> (usize, usize) {
        let without = |moved: Tally| {
            (
                staged.saturating_sub(moved.staged),
                total.saturating_sub(moved.total),
            )
        };
        match section {
            Section::Tracked => without(self.moved_from_tracked),
            Section::New => without(self.moved_from_new),
            Section::Changelist(id) => self
                .by_list
                .get(&id)
                .map_or((0, 0), |tally| (tally.staged, tally.total)),
            _ => (staged, total),
        }
    }

    /// Applies `change` to the shared lists. Does nothing without a local repository.
    fn update<R>(&mut self, cx: &mut App, change: impl FnOnce(&mut Changelists) -> R) -> Option<R> {
        let key = self.key.as_ref()?;
        let (model, result) = store::update(key, cx, change);
        self.model = model;
        Some(result)
    }
}

#[cfg(test)]
mod tests;
