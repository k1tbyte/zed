use super::{BranchesSidebar, operations::fetch_target};
use git::repository::Branch;
use gpui::{App, ClipboardItem, Context, Entity, SharedString, WeakEntity, Window};
use ui::{ContextMenu, prelude::*};

const MENU_NAME_LIMIT: usize = 28;

pub(super) fn branch_context_menu(
    sidebar: &WeakEntity<BranchesSidebar>,
    branch: &Branch,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    type Operation = fn(&mut BranchesSidebar, &Branch, &mut Window, &mut Context<BranchesSidebar>);
    let operation_entry = |menu: ContextMenu, label: SharedString, operation: Operation| {
        let (sidebar, branch) = (sidebar.clone(), branch.clone());
        menu.entry(label, None, move |window, cx| {
            sidebar
                .update(cx, |sidebar, cx| operation(sidebar, &branch, window, cx))
                .ok();
        })
    };
    let is_remote = branch.is_remote();
    let name = branch.name().to_string();

    let selection = sidebar
        .upgrade()
        .map(|sidebar| sidebar.read(cx).selected_branches(cx))
        .unwrap_or_default();
    if selection.len() > 1
        && selection
            .iter()
            .any(|selected| selected.ref_name == branch.ref_name)
    {
        let sidebar = sidebar.clone();
        return ContextMenu::build(window, cx, move |menu, _, _| {
            let deletable = selection.iter().filter(|branch| !branch.is_head).count();
            menu.header(format!("{} branches selected", selection.len()))
                .when(deletable > 0, |menu| {
                    menu.entry(
                        format!("Delete {deletable} Branches"),
                        None,
                        move |window, cx| {
                            let branches = selection.clone();
                            sidebar
                                .update(cx, |sidebar, cx| {
                                    sidebar.delete_branches(branches, window, cx)
                                })
                                .ok();
                        },
                    )
                })
        });
    }

    let is_local = sidebar
        .upgrade()
        .is_some_and(|sidebar| sidebar.read(cx).is_local(cx));
    let can_update = is_local
        && !is_remote
        && branch
            .upstream
            .as_ref()
            .is_some_and(|upstream| upstream.remote_name().is_some());
    let can_fetch = is_local && fetch_target(branch).is_some();
    let rebase_label = sidebar
        .upgrade()
        .and_then(|sidebar| sidebar.read(cx).current_branch(cx))
        .filter(|current| is_local && current.ref_name != branch.ref_name)
        .map(|current| {
            format!(
                "Rebase '{}' onto '{}'",
                truncate_middle(current.name(), MENU_NAME_LIMIT),
                truncate_middle(branch.name(), MENU_NAME_LIMIT)
            )
        });
    ContextMenu::build(window, cx, |menu, _, _| {
        menu.when(!is_remote && !branch.is_head, |menu| {
            operation_entry(menu, "Checkout".into(), BranchesSidebar::checkout)
        })
        .when(can_update, |menu| {
            operation_entry(menu, "Update".into(), BranchesSidebar::update)
        })
        .when(can_fetch, |menu| {
            operation_entry(menu, "Fetch".into(), BranchesSidebar::fetch)
        })
        .when(is_local && !is_remote, |menu| {
            operation_entry(menu, "Push…".into(), BranchesSidebar::push)
        })
        .map(|menu| {
            operation_entry(
                menu,
                "New Branch from Here…".into(),
                BranchesSidebar::new_branch_from,
            )
        })
        .when_some(rebase_label, |menu, label| {
            operation_entry(menu, label.into(), BranchesSidebar::rebase_current_onto)
        })
        .when(!is_remote, |menu| {
            operation_entry(menu, "Rename…".into(), BranchesSidebar::rename)
        })
        .when(!branch.is_head, |menu| {
            let label = if is_remote {
                "Delete Local Tracking Branch"
            } else {
                "Delete"
            };
            operation_entry(menu, label.into(), BranchesSidebar::delete)
        })
        .separator()
        .entry("Copy Branch Name", None, move |_, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(name.clone()));
        })
    })
}

// Keeps both ends of a long name: the prefix and the ticket id at the end matter most.
fn truncate_middle(text: &str, max_chars: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars {
        return text.to_string();
    }
    let kept = max_chars.saturating_sub(1);
    let (head, tail) = (kept.div_ceil(2), kept / 2);
    let start: String = chars.iter().take(head).collect();
    let end: String = chars.iter().skip(chars.len() - tail).collect();
    format!("{start}…{end}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_long_names_in_the_middle() {
        assert_eq!(truncate_middle("master", 10), "master");
        let name = "feature/DCXCONNEX-4164-template-action-drag-and-drop-words";
        let truncated = truncate_middle(name, 24);
        assert_eq!(truncated.chars().count(), 24);
        assert!(truncated.starts_with("feature/") && truncated.ends_with("words"));
        assert!(truncated.contains('…'));
        assert_eq!(truncate_middle("abcdef", 1), "…");
    }
}
