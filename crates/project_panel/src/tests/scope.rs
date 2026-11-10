#![cfg(test)]

use gpui::{BorrowAppContext, Entity, TestAppContext, VisualTestContext};
use language::DiagnosticSeverity;
use project::{FakeFs, Project};
use serde_json::json;
use settings::{ProjectPanelSettingsContent, SettingsStore};
use util::rel_path::rel_path;
use workspace::MultiWorkspace;

use crate::project_panel_tests::{
    find_project_entry, init_test, select_path, visible_entries_as_strings,
};
use crate::{
    CollapseAllEntries, CollapseSelectedEntry, ProjectPanel, SelectNextDiagnostic, SelectParent,
    scope::*,
};

async fn panel_with_monorepo(cx: &mut TestAppContext) -> (Entity<ProjectPanel>, VisualTestContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/root",
        json!({
            "apps": {
                "api": { "lib.rs": "" },
                "web": { "src": { "main.ts": "" }, "package.json": "" },
            },
            "libs": { "ui": { "button.ts": "" } },
            "README.md": "",
        }),
    )
    .await;
    let project = Project::test(fs, ["/root".as_ref()], cx).await;
    let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    let panel = workspace.update_in(&mut cx, ProjectPanel::new);
    cx.run_until_parked();
    (panel, cx)
}

fn scope_to(panel: &Entity<ProjectPanel>, path: &str, cx: &mut VisualTestContext) {
    select_path(panel, path, cx);
    panel.update_in(cx, |panel, window, cx| {
        panel.scope_to_folder(&ScopeToFolder, window, cx)
    });
    cx.run_until_parked();
}

fn update_panel_settings(
    cx: &mut VisualTestContext,
    update: impl FnOnce(&mut ProjectPanelSettingsContent),
) {
    cx.update(|_, cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |settings| {
                update(settings.project_panel.get_or_insert_default())
            });
        })
    });
    cx.run_until_parked();
}

#[gpui::test]
async fn test_scope_navigation(cx: &mut TestAppContext) {
    let (panel, mut cx) = panel_with_monorepo(cx).await;
    let cx = &mut cx;

    scope_to(&panel, "root/apps/web", cx);
    assert_eq!(
        visible_entries_as_strings(&panel, 0..10, cx),
        &[
            "v apps/web  <== selected",
            "    > src",
            "      package.json"
        ]
    );

    panel.update_in(cx, |panel, window, cx| {
        panel.scope_to_parent(&ScopeToParent, window, cx)
    });
    cx.run_until_parked();
    assert_eq!(
        visible_entries_as_strings(&panel, 0..10, cx),
        &[
            "v apps",
            "    > api",
            "    v web  <== selected",
            "        > src",
            "          package.json",
        ]
    );

    panel.update_in(cx, |panel, window, cx| {
        panel.scope_to_parent(&ScopeToParent, window, cx)
    });
    cx.run_until_parked();
    assert_eq!(
        visible_entries_as_strings(&panel, 0..10, cx),
        &[
            "v root",
            "    v apps  <== selected",
            "        > api",
            "        v web",
            "            > src",
            "              package.json",
            "    > libs",
            "      README.md",
        ]
    );
}

#[gpui::test]
async fn test_scope_root_is_not_folded(cx: &mut TestAppContext) {
    let (panel, mut cx) = panel_with_monorepo(cx).await;
    let cx = &mut cx;
    update_panel_settings(cx, |settings| settings.auto_fold_dirs = Some(true));
    select_path(&panel, "root/libs/ui", cx);
    assert_eq!(
        visible_entries_as_strings(&panel, 0..10, cx),
        &[
            "v root",
            "    > apps",
            "    > libs/ui  <== selected",
            "      README.md"
        ]
    );

    scope_to(&panel, "root/libs", cx);
    assert_eq!(
        visible_entries_as_strings(&panel, 0..10, cx),
        &["v libs  <== selected", "    > ui"]
    );
}

#[gpui::test]
async fn test_navigation_stays_in_scope(cx: &mut TestAppContext) {
    let (panel, mut cx) = panel_with_monorepo(cx).await;
    let cx = &mut cx;
    scope_to(&panel, "root/apps/web", cx);

    for _ in 0..2 {
        panel.update_in(cx, |panel, window, cx| {
            panel.collapse_selected_entry(&CollapseSelectedEntry, window, cx)
        });
    }
    panel.update_in(cx, |panel, window, cx| {
        panel.select_parent(&SelectParent, window, cx)
    });
    cx.run_until_parked();
    assert_eq!(
        visible_entries_as_strings(&panel, 0..10, cx),
        &["> apps/web  <== selected"]
    );

    panel.update_in(cx, |panel, window, cx| {
        panel.collapse_all_entries(&CollapseAllEntries, window, cx)
    });
    cx.run_until_parked();
    assert_eq!(
        visible_entries_as_strings(&panel, 0..10, cx),
        &[
            "v apps/web  <== selected",
            "    > src",
            "      package.json"
        ],
        "Collapse All keeps the scope root expanded"
    );
}

#[gpui::test]
async fn test_diagnostic_navigation_stays_in_scope(cx: &mut TestAppContext) {
    let (panel, mut cx) = panel_with_monorepo(cx).await;
    let cx = &mut cx;
    panel.update(cx, |panel, cx| {
        let worktree_id = panel
            .project
            .read(cx)
            .worktrees(cx)
            .next()
            .unwrap()
            .read(cx)
            .id();
        for path in ["apps/web/package.json", "libs/ui/button.ts"] {
            panel.diagnostics.insert(
                (worktree_id, rel_path(path).into()),
                DiagnosticSeverity::ERROR,
            );
        }
    });
    scope_to(&panel, "root/apps/web", cx);

    for _ in 0..2 {
        panel.update_in(cx, |panel, window, cx| {
            panel.select_next_diagnostic(&SelectNextDiagnostic::default(), window, cx)
        });
        cx.run_until_parked();
    }
    assert_eq!(
        visible_entries_as_strings(&panel, 0..10, cx),
        &[
            "v apps/web",
            "    > src",
            "      package.json  <== selected"
        ]
    );
}

#[gpui::test]
async fn test_reveal_outside_scope(cx: &mut TestAppContext) {
    let (panel, mut cx) = panel_with_monorepo(cx).await;
    let cx = &mut cx;
    update_panel_settings(cx, |settings| settings.auto_reveal_entries = Some(true));

    scope_to(&panel, "root/apps/web", cx);
    let scoped = &[
        "v apps/web  <== selected",
        "    > src",
        "      package.json",
    ];

    let lib = find_project_entry(&panel, "root/apps/api/lib.rs", cx).unwrap();
    panel.update(cx, |panel, cx| {
        panel.project.update(cx, |_, cx| {
            cx.emit(project::Event::ActiveEntryChanged(Some(lib)))
        })
    });
    cx.run_until_parked();
    assert_eq!(
        visible_entries_as_strings(&panel, 0..10, cx),
        scoped,
        "Auto-reveal keeps the scope"
    );

    panel.update(cx, |panel, cx| {
        panel.project.update(cx, |_, cx| {
            cx.emit(project::Event::RevealInProjectPanel(lib))
        })
    });
    cx.run_until_parked();
    assert_eq!(
        visible_entries_as_strings(&panel, 0..10, cx),
        &[
            "v root",
            "    v apps",
            "        v api",
            "              lib.rs  <== selected  <== marked",
            "        v web",
            "            > src",
            "              package.json",
            "    > libs",
            "      README.md",
        ],
        "Explicit reveal resets the scope"
    );
}

#[gpui::test]
async fn test_saved_scope_is_restored_once_its_folder_is_scanned(cx: &mut TestAppContext) {
    let (panel, mut cx) = panel_with_monorepo(cx).await;
    let cx = &mut cx;
    let whole_project = visible_entries_as_strings(&panel, 0..10, cx);

    panel.update_in(cx, |panel, window, cx| {
        panel.apply_saved_scope("/root/apps/late".into(), window, cx)
    });
    cx.run_until_parked();
    assert_eq!(visible_entries_as_strings(&panel, 0..10, cx), whole_project);

    let fs = panel.read_with(cx, |panel, cx| panel.project.read(cx).fs().as_fake());
    fs.insert_tree("/root/apps/late", json!({ "a.rs": "" }))
        .await;
    cx.run_until_parked();
    assert_eq!(
        visible_entries_as_strings(&panel, 0..10, cx),
        &["v apps/late", "      a.rs"]
    );

    panel.update_in(cx, |panel, window, cx| {
        panel.apply_saved_scope("/root/apps/later".into(), window, cx);
        panel.reset_scope(&ResetScope, window, cx);
    });
    fs.insert_tree("/root/apps/later", json!({ "b.rs": "" }))
        .await;
    cx.run_until_parked();
    panel.read_with(cx, |panel, _| {
        assert!(
            panel.scope.is_none(),
            "A user reset drops the pending scope"
        )
    });
}
