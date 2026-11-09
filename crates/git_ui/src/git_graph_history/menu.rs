use super::{HistoryTarget, flows::ResetKind};
use gpui::{App, Window};
use ui::{ContextMenu, ContextMenuEntry, DocumentationSide, prelude::*};

type Action = fn(&HistoryTarget, &mut Window, &mut App);

/// Appended to the single-commit context menu.
pub(crate) fn history_entries(menu: ContextMenu, target: HistoryTarget) -> ContextMenu {
    let rewrite = target.rewrite_blocker;
    let no_previous = target
        .previous
        .is_none()
        .then_some("There is no earlier commit to fold into");
    let reset_target = target.clone();
    let branch_target = target.clone();
    menu.separator()
        .when(target.is_local, |menu| {
            let menu = entry(
                menu,
                "Edit Commit Message…",
                &target,
                rewrite,
                HistoryTarget::edit_message,
            );
            let menu = entry(
                menu,
                "Squash into Previous",
                &target,
                rewrite.or(no_previous),
                HistoryTarget::squash_into_previous,
            );
            shared_entries(menu, &target)
        })
        .entry("New Branch from Here…", None, move |window, cx| {
            branch_target.new_branch(window, cx)
        })
        .submenu("Reset Current Branch to Here", move |menu, _, _| {
            reset_entries(menu, &reset_target)
        })
}

/// The whole menu for a multi-row selection.
pub(crate) fn selection_menu(menu: ContextMenu, target: HistoryTarget) -> ContextMenu {
    let rewrite = target.rewrite_blocker;
    let copy_target = target.clone();
    menu.header(format!("{} commits", target.shas.len()))
        .when(target.is_local, |menu| {
            let menu = entry(
                menu,
                "Squash Commits…",
                &target,
                rewrite,
                HistoryTarget::squash,
            );
            shared_entries(menu, &target)
        })
        .entry("Copy SHAs", None, move |_, cx| copy_target.copy_shas(cx))
}

// Reads the same for one commit and for several.
fn shared_entries(menu: ContextMenu, target: &HistoryTarget) -> ContextMenu {
    let [drop, revert, cherry_pick] = if target.shas.len() > 1 {
        ["Drop Commits", "Revert Commits", "Cherry-pick Commits"]
    } else {
        ["Drop Commit", "Revert Commit", "Cherry-pick Commit"]
    };
    let menu = entry(
        menu,
        drop,
        target,
        target.rewrite_blocker,
        HistoryTarget::drop_commits,
    );
    let menu = entry(
        menu.separator(),
        revert,
        target,
        None,
        HistoryTarget::revert,
    );
    entry(menu, cherry_pick, target, None, HistoryTarget::cherry_pick).separator()
}

fn reset_entries(menu: ContextMenu, target: &HistoryTarget) -> ContextMenu {
    let mut kinds = vec![
        ("Soft: keep changes staged", ResetKind::Soft),
        ("Mixed: keep changes unstaged", ResetKind::Mixed),
    ];
    if target.is_local {
        kinds.extend([
            (
                "Keep: keep local changes, fail on conflicts",
                ResetKind::Keep,
            ),
            ("Hard: discard all changes…", ResetKind::Hard),
        ]);
    }
    kinds.into_iter().fold(menu, |menu, (label, kind)| {
        let target = target.clone();
        menu.entry(label, None, move |window, cx| {
            target.reset(kind, window, cx)
        })
    })
}

// A blocked entry stays visible and says why on hover.
fn entry(
    menu: ContextMenu,
    label: &'static str,
    target: &HistoryTarget,
    blocker: Option<&'static str>,
    action: Action,
) -> ContextMenu {
    let target = target.clone();
    let entry = ContextMenuEntry::new(label).disabled(blocker.is_some());
    match blocker {
        Some(reason) => menu.item(
            entry.documentation_aside(DocumentationSide::Right, move |_| {
                Label::new(reason).into_any_element()
            }),
        ),
        None => menu.item(entry.handler(move |window, cx| action(&target, window, cx))),
    }
}
