use super::*;
use crate::git_panel::{GitHeaderEntry, GitListEntry, StageIntent, UPDATE_DEBOUNCE};
use git::{
    repository::repo_path,
    status::{FileStatus, StageStatus, StatusCode},
};
use gpui::{Task, TestAppContext, VisualContext as _, VisualTestContext};
use project::{FakeFs, Project};
use serde_json::json;
use settings::SettingsStore;
use std::{path::Path, sync::Arc};
use theme::LoadThemes;
use ui::prelude::ToggleState;
use util::path;
use workspace::MultiWorkspace;

fn statuses() -> [(&'static str, FileStatus); 4] {
    [
        ("a.rs", StatusCode::Modified.worktree()),
        ("b.rs", StatusCode::Modified.worktree()),
        ("c.rs", StatusCode::Modified.worktree()),
        ("n.txt", FileStatus::Untracked),
    ]
}

fn init_test(cx: &mut TestAppContext) {
    zlog::init_test();
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        theme_settings::init(LoadThemes::JustBase, cx);
        language_model::init(cx);
        editor::init(cx);
        crate::init(cx);
    });
}

/// Lists persist per repository path in a store shared by all tests, so each test gets its own.
async fn setup(
    cx: &mut TestAppContext,
    project_dir: &str,
) -> (
    Arc<FakeFs>,
    Entity<Project>,
    Entity<GitPanel>,
    VisualTestContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.background_executor.clone());
    let root = format!("{}/{project_dir}", path!("/root"));
    fs.insert_tree(
        &root,
        json!({
            ".git": {},
            "a.rs": "a", "b.rs": "b", "c.rs": "c", "n.txt": "n"
        }),
    )
    .await;
    fs.set_status_for_repo(Path::new(&format!("{root}/.git")), &statuses());

    let project = Project::test(fs.clone(), [Path::new(&root)], cx).await;
    let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    let panel = workspace.update_in(&mut cx, GitPanel::new);
    settle(&panel, &project, &mut cx).await;
    (fs, project, panel, cx)
}

async fn settle(panel: &Entity<GitPanel>, project: &Entity<Project>, cx: &mut VisualTestContext) {
    cx.read(|cx| {
        project
            .read(cx)
            .worktrees(cx)
            .next()
            .unwrap()
            .read(cx)
            .as_local()
            .unwrap()
            .scan_complete()
    })
    .await;
    cx.executor().run_until_parked();
    let rebuild = cx.update_window_entity(panel, |panel, _, _| {
        std::mem::replace(&mut panel.update_visible_entries_task, Task::ready(()))
    });
    cx.executor().advance_clock(2 * UPDATE_DEBOUNCE);
    rebuild.await;
}

/// Headers and files in display order, e.g. `["Tracked", "a.rs U", "Changelist", "b.rs S"]`.
fn rows(panel: &Entity<GitPanel>, cx: &mut VisualTestContext) -> Vec<String> {
    panel.read_with(cx, |panel, _| {
        panel
            .entries
            .iter()
            .filter_map(|entry| match entry {
                GitListEntry::Header(header) => Some(header.title().to_string()),
                GitListEntry::Status(status) => Some(format!(
                    "{} {}",
                    status.repo_path.as_unix_str(),
                    match status.staging {
                        StageStatus::Staged => "S",
                        StageStatus::Unstaged => "U",
                        StageStatus::PartiallyStaged => "P",
                    }
                )),
                _ => None,
            })
            .collect()
    })
}

fn create_list(panel: &Entity<GitPanel>, name: &str, cx: &mut VisualTestContext) -> ChangelistId {
    panel.update_in(cx, |panel, _, cx| {
        panel
            .changelists
            .update(cx, |model| model.create(name))
            .flatten()
            .expect("a local repository is active")
    })
}

fn rebuild(panel: &Entity<GitPanel>, cx: &mut VisualTestContext) {
    panel.update_in(cx, |panel, window, cx| {
        panel.update_visible_entries(window, cx)
    });
}

fn assign(
    panel: &Entity<GitPanel>,
    files: &[&str],
    list: Option<ChangelistId>,
    cx: &mut VisualTestContext,
) {
    let paths = files.iter().map(|file| repo_path(file)).collect();
    panel.update_in(cx, |panel, window, cx| {
        panel.assign_changes(paths, list, window, cx)
    });
}

#[gpui::test]
async fn lists_regroup_files_and_their_header_stages_only_members(cx: &mut TestAppContext) {
    let (_fs, project, panel, mut cx) = setup(cx, "regroup").await;
    assert_eq!(
        rows(&panel, &mut cx),
        [
            "Tracked",
            "a.rs U",
            "b.rs U",
            "c.rs U",
            "Untracked",
            "n.txt U"
        ]
    );

    let local = create_list(&panel, "Local", &mut cx);
    assign(&panel, &["b.rs", "n.txt"], Some(local), &mut cx);
    assert_eq!(
        rows(&panel, &mut cx),
        [
            "Tracked",
            "a.rs U",
            "c.rs U",
            "Changelist",
            "b.rs U",
            "n.txt U"
        ]
    );

    panel.update_in(&mut cx, |panel, window, cx| {
        let header = GitListEntry::Header(GitHeaderEntry {
            header: Section::Changelist(local),
        });
        panel.toggle_staged_for_entry(&header, StageIntent::Toggle, window, cx);
    });
    settle(&panel, &project, &mut cx).await;

    assert_eq!(
        rows(&panel, &mut cx),
        [
            "Tracked",
            "a.rs U",
            "c.rs U",
            "Changelist",
            "b.rs S",
            "n.txt S"
        ]
    );
    panel.read_with(&cx, |panel, _| {
        assert_eq!(
            panel.header_state(Section::Changelist(local)),
            ToggleState::Selected
        );
        assert_eq!(
            panel.header_state(Section::Tracked),
            ToggleState::Unselected
        );
    });
}

#[gpui::test]
async fn an_empty_list_stays_and_deleting_a_list_returns_its_files(cx: &mut TestAppContext) {
    let (_fs, _project, panel, mut cx) = setup(cx, "empty_list").await;
    let spare = create_list(&panel, "Spare", &mut cx);
    rebuild(&panel, &mut cx);
    assert_eq!(
        rows(&panel, &mut cx),
        [
            "Tracked",
            "a.rs U",
            "b.rs U",
            "c.rs U",
            "Changelist",
            "Untracked",
            "n.txt U"
        ]
    );

    assign(&panel, &["a.rs"], Some(spare), &mut cx);
    panel.update_in(&mut cx, |panel, window, cx| {
        panel.delete_changelist(spare, window, cx)
    });
    assert_eq!(
        rows(&panel, &mut cx),
        [
            "Tracked",
            "a.rs U",
            "b.rs U",
            "c.rs U",
            "Untracked",
            "n.txt U"
        ]
    );
}

#[gpui::test]
async fn files_without_changes_leave_their_list(cx: &mut TestAppContext) {
    let (fs, project, panel, mut cx) = setup(cx, "prune").await;
    let local = create_list(&panel, "Local", &mut cx);
    assign(&panel, &["b.rs", "c.rs"], Some(local), &mut cx);

    fs.set_status_for_repo(
        Path::new(&format!("{}/prune/.git", path!("/root"))),
        &[
            ("a.rs", StatusCode::Modified.worktree()),
            ("c.rs", StatusCode::Modified.worktree()),
        ],
    );
    settle(&panel, &project, &mut cx).await;

    panel.read_with(&cx, |panel, _| {
        assert_eq!(panel.changelists.model().list_of(&repo_path("b.rs")), None);
        assert_eq!(
            panel.changelists.model().list_of(&repo_path("c.rs")),
            Some(local)
        );
    });
}

#[gpui::test]
async fn tracked_stays_as_a_drop_target_when_every_file_is_in_a_list(cx: &mut TestAppContext) {
    let (_fs, _project, panel, mut cx) = setup(cx, "all_listed").await;
    let local = create_list(&panel, "Local", &mut cx);
    assign(
        &panel,
        &["a.rs", "b.rs", "c.rs", "n.txt"],
        Some(local),
        &mut cx,
    );
    assert_eq!(
        rows(&panel, &mut cx),
        [
            "Tracked",
            "Changelist",
            "a.rs U",
            "b.rs U",
            "c.rs U",
            "n.txt U"
        ]
    );

    assign(&panel, &["a.rs"], None, &mut cx);
    assert_eq!(
        rows(&panel, &mut cx),
        [
            "Tracked",
            "a.rs U",
            "Changelist",
            "b.rs U",
            "c.rs U",
            "n.txt U"
        ]
    );
}

#[gpui::test]
async fn rows_dragged_as_a_selection_drop_their_files_on_a_list(cx: &mut TestAppContext) {
    let (_fs, _project, panel, mut cx) = setup(cx, "drag_selection").await;
    // Rows: 0 Tracked, 1 a.rs, 2 b.rs, 3 c.rs, 4 Untracked, 5 n.txt
    let dropped = |panel: &GitPanel, ix: usize, cx: &gpui::App| {
        let entry = panel.entries[ix].status_entry().unwrap().clone();
        let repo = panel.active_repository.as_ref().unwrap().read(cx);
        let selection = panel.dragged_selection(ix, &entry, repo, cx).unwrap();
        panel.selection_paths(&selection, cx)
    };
    panel.update_in(&mut cx, |panel, _, cx| {
        assert_eq!(dropped(panel, 3, cx), [repo_path("c.rs")]);
        panel.toggle_mark(1, cx);
        panel.toggle_mark(2, cx);
        assert_eq!(dropped(panel, 1, cx), [repo_path("a.rs"), repo_path("b.rs")]);
    });
}

#[gpui::test]
async fn dragging_a_marked_row_ignores_the_keyboard_selection(cx: &mut TestAppContext) {
    let (_fs, _project, panel, mut cx) = setup(cx, "drag_payload").await;
    // Rows: 0 Tracked, 1 a.rs, 2 b.rs, 3 c.rs, 4 Untracked, 5 n.txt
    panel.update_in(&mut cx, |panel, _, cx| {
        panel.toggle_mark(1, cx);
        panel.selected_entry = Some(2);
    });

    panel.read_with(&cx, |panel, _| {
        let entry = |ix: usize| panel.entries[ix].status_entry().unwrap().clone();
        let dragged = |ix: usize| panel.dragged_paths(&panel.dragged_file(ix, &entry(ix)));
        assert_eq!(dragged(1), [repo_path("a.rs")]);
        assert_eq!(dragged(3), [repo_path("c.rs")]);
    });
}
