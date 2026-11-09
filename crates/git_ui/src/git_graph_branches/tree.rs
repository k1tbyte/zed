use git::repository::Branch;
use gpui::SharedString;
use std::collections::HashSet;

const LOCAL_KEY: &str = "local";
const REMOTE_KEY: &str = "remote";

pub(super) enum Row {
    Head,
    Folder {
        key: SharedString,
        name: SharedString,
        depth: usize,
        expanded: bool,
    },
    Branch {
        branch: Branch,
        label: SharedString,
        depth: usize,
    },
}

impl Row {
    pub(super) fn depth(&self) -> usize {
        match self {
            Row::Head => 0,
            Row::Folder { depth, .. } | Row::Branch { depth, .. } => *depth,
        }
    }

    // Rough width in characters, used to pick the row that sizes the horizontally scrolling list.
    pub(super) fn width_hint(&self) -> usize {
        match self {
            Row::Head => 22,
            Row::Folder { name, depth, .. } => depth * 2 + name.chars().count() + 2,
            Row::Branch {
                branch,
                label,
                depth,
            } => {
                let badges = if branch.tracking_status().is_some() {
                    8
                } else {
                    0
                };
                depth * 2 + label.chars().count() + 4 + badges
            }
        }
    }
}

// Branch rows between `anchor` and `target` in visible order, folders skipped.
pub(super) fn refs_between(
    rows: &[Row],
    anchor: Option<&SharedString>,
    target: &SharedString,
) -> Option<HashSet<SharedString>> {
    let refs: Vec<&SharedString> = rows
        .iter()
        .filter_map(|row| match row {
            Row::Branch { branch, .. } => Some(&branch.ref_name),
            _ => None,
        })
        .collect();
    let position =
        |ref_name: &SharedString| refs.iter().position(|candidate| *candidate == ref_name);
    let target = position(target)?;
    let anchor = anchor.and_then(position).unwrap_or(target);
    let (start, end) = (anchor.min(target), anchor.max(target));
    Some(
        refs.iter()
            .skip(start)
            .take(end - start + 1)
            .map(|ref_name| (*ref_name).clone())
            .collect(),
    )
}

pub(super) fn build_rows(
    branches: &[Branch],
    filter: &str,
    collapsed: &HashSet<SharedString>,
) -> Vec<Row> {
    let filter = filter.to_lowercase();
    let mut rows = vec![Row::Head];
    for (key, label, is_remote) in [(LOCAL_KEY, "Local", false), (REMOTE_KEY, "Remote", true)] {
        let mut section: Vec<&Branch> = branches
            .iter()
            .filter(|branch| branch.is_remote() == is_remote)
            .filter(|branch| !(is_remote && branch.name().ends_with("/HEAD")))
            .filter(|branch| branch.name().to_lowercase().contains(&filter))
            .collect();
        if section.is_empty() {
            continue;
        }
        section.sort_by(|a, b| a.name().cmp(b.name()));

        let force_open = !filter.is_empty();
        let expanded = force_open || !collapsed.contains(key);
        rows.push(Row::Folder {
            key: key.into(),
            name: label.into(),
            depth: 0,
            expanded,
        });
        if expanded {
            push_tree(&mut rows, key, &section, force_open, collapsed);
        }
    }
    rows
}

fn push_tree(
    rows: &mut Vec<Row>,
    section_key: &str,
    branches: &[&Branch],
    force_open: bool,
    collapsed: &HashSet<SharedString>,
) {
    let mut open_dirs: Vec<&str> = Vec::new();
    let mut hidden_below: Option<usize> = None;
    for branch in branches {
        let components: Vec<&str> = branch.name().split('/').collect();
        let Some((leaf, dirs)) = components.split_last() else {
            continue;
        };
        let shared = open_dirs
            .iter()
            .zip(dirs)
            .take_while(|(a, b)| a == b)
            .count();
        open_dirs.truncate(shared);
        if hidden_below.is_some_and(|depth| shared <= depth) {
            hidden_below = None;
        }
        for (depth, dir) in dirs.iter().enumerate().skip(shared) {
            open_dirs.push(dir);
            if hidden_below.is_some() {
                continue;
            }
            let key = SharedString::from(format!("{section_key}/{}", dirs[..=depth].join("/")));
            let expanded = force_open || !collapsed.contains(&key);
            if !expanded {
                hidden_below = Some(depth);
            }
            rows.push(Row::Folder {
                key,
                name: (*dir).into(),
                depth: depth + 1,
                expanded,
            });
        }
        if hidden_below.is_none() {
            rows.push(Row::Branch {
                branch: (*branch).clone(),
                label: (*leaf).into(),
                depth: dirs.len() + 1,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn branch(ref_name: &str) -> Branch {
        Branch {
            is_head: false,
            ref_name: ref_name.to_string().into(),
            upstream: None,
            most_recent_commit: None,
        }
    }

    fn outline(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                Row::Head => "HEAD".to_string(),
                Row::Folder { name, depth, .. } => format!("{}{name}/", " ".repeat(*depth)),
                Row::Branch { label, depth, .. } => format!("{}{label}", " ".repeat(*depth)),
            })
            .collect()
    }

    fn sample() -> Vec<Branch> {
        [
            "refs/heads/master",
            "refs/heads/feature/a",
            "refs/heads/feature/b",
            "refs/remotes/origin/main",
            "refs/remotes/origin/HEAD",
        ]
        .map(branch)
        .to_vec()
    }

    #[test]
    fn groups_branches_into_folders() {
        let rows = build_rows(&sample(), "", &HashSet::new());
        assert_eq!(
            outline(&rows),
            [
                "HEAD",
                "Local/",
                " feature/",
                "  a",
                "  b",
                " master",
                "Remote/",
                " origin/",
                "  main"
            ]
        );
    }

    #[test]
    fn collapsed_folders_hide_their_children() {
        let collapsed = HashSet::from([SharedString::from("local/feature"), "remote".into()]);
        let rows = build_rows(&sample(), "", &collapsed);
        assert_eq!(
            outline(&rows),
            ["HEAD", "Local/", " feature/", " master", "Remote/"]
        );
    }

    #[test]
    fn shift_range_spans_visible_branches_in_either_direction() {
        let rows = build_rows(&sample(), "", &HashSet::new());
        let name = |ref_name: &str| SharedString::from(ref_name.to_string());
        let (first, last) = (name("refs/heads/feature/a"), name("refs/heads/master"));

        let forward = refs_between(&rows, Some(&first), &last);
        let backward = refs_between(&rows, Some(&last), &first);
        assert_eq!(forward, backward);
        assert_eq!(forward.map(|refs| refs.len()), Some(3));

        let without_anchor = refs_between(&rows, None, &last);
        assert_eq!(without_anchor, Some(HashSet::from([last])));
        assert_eq!(refs_between(&rows, None, &name("refs/heads/gone")), None);
    }

    #[test]
    fn filter_narrows_and_ignores_collapsed_state() {
        let collapsed = HashSet::from([SharedString::from("local/feature")]);
        let rows = build_rows(&sample(), "FEATURE/b", &collapsed);
        assert_eq!(outline(&rows), ["HEAD", "Local/", " feature/", "  b"]);
    }
}
