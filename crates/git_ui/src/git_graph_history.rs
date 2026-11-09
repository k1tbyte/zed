//! JetBrains-style history editing for the commits selected in the git graph.

mod flows;
mod menu;
mod selection;

pub(crate) use menu::{history_entries, selection_menu};
pub(crate) use selection::{Pick, RowSelection};

use crate::{git_graph_branches::BranchesSidebar, local_git::local_repo};
use git::{Oid, repository::InitialGraphCommitData};
use gpui::{App, Entity, WeakEntity};
use project::git_store::Repository;
use std::{rc::Rc, str::FromStr as _};
use workspace::Workspace;

/// What the graph knew about the selected commits when its context menu opened.
#[derive(Clone)]
pub(crate) struct HistoryTarget {
    /// Newest first, as the rows are ordered.
    shas: Rc<[Oid]>,
    repository: Entity<Repository>,
    workspace: WeakEntity<Workspace>,
    sidebar: WeakEntity<BranchesSidebar>,
    is_local: bool,
    /// Why the history cannot be rewritten here; the authoritative checks run again in git_ops.
    rewrite_blocker: Option<&'static str>,
    /// The commit before a single selected one, for folding it away.
    previous: Option<Oid>,
}

impl HistoryTarget {
    pub(crate) fn new<'a>(
        repository: Entity<Repository>,
        workspace: WeakEntity<Workspace>,
        sidebar: WeakEntity<BranchesSidebar>,
        shas: Vec<Oid>,
        rows: impl Iterator<Item = &'a InitialGraphCommitData>,
        cx: &mut App,
    ) -> Self {
        let is_local = local_repo(&repository, cx).is_some();
        let (head, on_branch) = {
            let repository = repository.read(cx);
            let head = repository
                .head_commit
                .as_ref()
                .and_then(|commit| Oid::from_str(&commit.sha).ok());
            (head, repository.branch.is_some())
        };
        let chain = head.map(|head| first_parent_chain(rows, head));
        let (rewrite_blocker, previous) = match (&chain, on_branch) {
            (_, false) => (Some("HEAD is detached"), None),
            (None, _) => (Some("The branch has no commits"), None),
            (Some(chain), true) => rewrite_check(chain, &shas),
        };
        Self {
            shas: shas.into(),
            repository,
            workspace,
            sidebar,
            is_local,
            rewrite_blocker,
            previous,
        }
    }
}

/// HEAD's first-parent line among the loaded rows, as (sha, parent count), newest first.
fn first_parent_chain<'a>(
    rows: impl Iterator<Item = &'a InitialGraphCommitData>,
    head: Oid,
) -> Vec<(Oid, usize)> {
    let mut wanted = Some(head);
    let mut chain = Vec::new();
    for row in rows {
        if wanted != Some(row.sha) {
            continue;
        }
        chain.push((row.sha, row.parents.len()));
        wanted = row.parents.first().copied();
        if wanted.is_none() {
            break;
        }
    }
    chain
}

fn rewrite_check(chain: &[(Oid, usize)], shas: &[Oid]) -> (Option<&'static str>, Option<Oid>) {
    let position = |sha: &Oid| chain.iter().position(|(chain_sha, _)| chain_sha == sha);
    let Some(oldest) = shas
        .iter()
        .map(position)
        .collect::<Option<Vec<_>>>()
        .and_then(|positions| positions.into_iter().max())
    else {
        return (Some("Not on the current branch"), None);
    };
    if chain[..=oldest].iter().any(|(_, parents)| *parents > 1) {
        return (Some("A merge commit lies in this range"), None);
    }
    let previous = chain
        .get(oldest + 1)
        .filter(|(_, parents)| *parents <= 1)
        .map(|(sha, _)| *sha);
    (None, previous)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(number: u32) -> Oid {
        Oid::from_str(&format!("{number:040x}")).unwrap()
    }

    fn row(number: u32, parents: &[u32]) -> InitialGraphCommitData {
        InitialGraphCommitData {
            sha: oid(number),
            parents: parents.iter().copied().map(oid).collect(),
            ref_names: Vec::new(),
        }
    }

    // c4 merges `side` (7) into c3, so the first-parent line is 4, 3, 2, 1.
    fn rows() -> Vec<InitialGraphCommitData> {
        vec![
            row(4, &[3, 7]),
            row(7, &[2]),
            row(3, &[2]),
            row(2, &[1]),
            row(1, &[]),
        ]
    }

    fn check(head: u32, selected: &[u32]) -> (Option<&'static str>, Option<Oid>) {
        let rows = rows();
        let chain = first_parent_chain(rows.iter(), oid(head));
        let selected: Vec<Oid> = selected.iter().copied().map(oid).collect();
        rewrite_check(&chain, &selected)
    }

    #[test]
    fn follows_only_the_first_parent_line() {
        let rows = rows();
        let chain = first_parent_chain(rows.iter(), oid(4));
        let shas: Vec<Oid> = chain.iter().map(|(sha, _)| *sha).collect();
        assert_eq!(shas, [oid(4), oid(3), oid(2), oid(1)]);
    }

    #[test]
    fn commits_below_the_merge_can_be_rewritten_and_know_their_previous_commit() {
        assert_eq!(check(3, &[2]), (None, Some(oid(1))));
        assert_eq!(check(3, &[3, 2]), (None, Some(oid(1))));
    }

    #[test]
    fn the_root_has_nothing_to_fold_into() {
        assert_eq!(check(3, &[1]), (None, None));
    }

    #[test]
    fn a_merge_commit_in_the_range_blocks_the_rewrite() {
        assert_eq!(check(4, &[2]).0, Some("A merge commit lies in this range"));
        assert_eq!(check(4, &[4]).0, Some("A merge commit lies in this range"));
    }

    #[test]
    fn a_previous_commit_that_is_a_merge_is_not_offered() {
        let rows = [row(3, &[2]), row(2, &[1, 9]), row(1, &[])];
        let chain = first_parent_chain(rows.iter(), oid(3));
        assert_eq!(rewrite_check(&chain, &[oid(3)]), (None, None));
    }

    #[test]
    fn commits_of_other_branches_are_refused() {
        assert_eq!(check(3, &[7]).0, Some("Not on the current branch"));
        assert_eq!(check(3, &[2, 7]).0, Some("Not on the current branch"));
    }
}
