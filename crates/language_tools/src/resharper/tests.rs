use super::*;
use futures::StreamExt as _;
use gpui::{TestAppContext, VisualTestContext};
use language::{FakeLspAdapter, Language, LanguageConfig, LanguageMatcher};
use lsp::{DEFAULT_LSP_REQUEST_TIMEOUT, FakeLanguageServer};
use project::{Project, ProjectPath};
use serde_json::{Value, json};
use util::{path, rel_path::rel_path};
use workspace::{AppState, MultiWorkspace};

macro_rules! backend_request {
    ($name:ident, $method:literal) => {
        enum $name {}

        impl lsp::request::Request for $name {
            type Params = Value;
            type Result = Value;
            const METHOD: &'static str = $method;
        }
    };
}

backend_request!(
    ApplyChangesWithSnippet,
    "resharper/snippets/applyChangesWithSnippet"
);
backend_request!(PickOne, "resharper/quickPick/pickOne");
backend_request!(PickMany, "resharper/quickPick/pickMany");

async fn open_editor<'a>(
    text: &str,
    cx: &'a mut TestAppContext,
) -> (
    Entity<Workspace>,
    Entity<Editor>,
    FakeLanguageServer,
    &'a mut VisualTestContext,
) {
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        settings::init(cx);
        editor::init(cx);
        init(cx);
        app_state
    });
    let fs = app_state.fs.as_fake();
    fs.insert_tree(path!("/root"), json!({ "Program.cs": text }))
        .await;
    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
    let languages = project.read_with(cx, |project, _| project.languages().clone());
    languages.add(Arc::new(Language::new(
        LanguageConfig {
            name: "CSharp".into(),
            matcher: LanguageMatcher {
                path_suffixes: vec!["cs".into()],
                ..LanguageMatcher::default()
            }
            .into(),
            ..LanguageConfig::default()
        },
        None,
    )));
    let mut servers = languages.register_fake_lsp(
        "CSharp",
        FakeLspAdapter {
            name: "resharper",
            ..FakeLspAdapter::default()
        },
    );
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    let worktree_id = project.read_with(cx, |project, cx| {
        project.worktrees(cx).next().unwrap().read(cx).id()
    });
    let path = ProjectPath {
        worktree_id,
        path: rel_path("Program.cs").into(),
    };
    let item = workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_path(path, None, true, window, cx)
        })
        .await
        .unwrap();
    let editor = cx.update(|_, cx| item.act_as::<Editor>(cx).unwrap());
    let server = servers.next().await.unwrap();
    cx.run_until_parked();
    (workspace, editor, server, cx)
}

#[gpui::test]
async fn test_backend_edits_end_in_a_snippet(cx: &mut TestAppContext) {
    let original = "var x = Add(1, 2 + 3);\n";
    let (_workspace, editor, server, cx) = open_editor(original, cx).await;

    // What the backend sends for "Introduce variable" on `2 + 3`.
    server
        .request::<ApplyChangesWithSnippet>(
            json!({
                "edits": [
                    { "range": range(0, 15, 0, 20), "newText": "i" },
                    { "range": range(0, 0, 0, 0), "newText": "var i = 2 + 3;\n" },
                ],
                "snippetText": "${1|var,int|} ${2:i}$0 = 2 + 3;\nvar x = Add(1, ${2:i}",
                "snippetRange": range(0, 0, 1, 16),
            }),
            DEFAULT_LSP_REQUEST_TIMEOUT,
        )
        .await
        .into_response()
        .unwrap();

    let introduced = "var i = 2 + 3;\nvar x = Add(1, i);\n";
    editor.update_in(cx, |editor, window, cx| {
        assert_eq!(editor.text(cx), introduced);
        let selected = editor
            .selections
            .newest::<MultiBufferOffset>(&editor.display_snapshot(cx));
        assert_eq!(
            selected.range(),
            MultiBufferOffset(0)..MultiBufferOffset(3),
            "the first tab stop is the type"
        );

        editor.undo(&Default::default(), window, cx);
        assert_eq!(editor.text(cx), original, "one undo reverts the command");
    });
}

#[gpui::test]
async fn test_snippet_tab_stops_survive_crlf(cx: &mut TestAppContext) {
    let (_workspace, editor, server, cx) = open_editor("var x = 1;\r\nvar y = 2;\r\n", cx).await;

    server
        .request::<ApplyChangesWithSnippet>(
            json!({
                "edits": [],
                "snippetText": "var x = 1;\r\nvar ${1:y}",
                "snippetRange": range(0, 0, 1, 5),
            }),
            DEFAULT_LSP_REQUEST_TIMEOUT,
        )
        .await
        .into_response()
        .unwrap();

    editor.update(cx, |editor, cx| {
        assert_eq!(editor.text(cx), "var x = 1;\nvar y = 2;\n");
        let selected = editor
            .selections
            .newest::<MultiBufferOffset>(&editor.display_snapshot(cx));
        assert_eq!(
            selected.range(),
            MultiBufferOffset(15)..MultiBufferOffset(16)
        );
    });
}

#[gpui::test]
async fn test_backend_edits_fail_without_an_editor(cx: &mut TestAppContext) {
    let (workspace, editor, server, cx) = open_editor("var x = 1;\n", cx).await;
    drop(editor);
    workspace
        .update_in(cx, |workspace, window, cx| {
            let pane = workspace.active_pane().clone();
            pane.update(cx, |pane, cx| {
                pane.close_all_items(&Default::default(), window, cx)
            })
        })
        .await
        .unwrap();

    let result = server
        .request::<ApplyChangesWithSnippet>(
            json!({ "edits": [], "snippetText": "x", "snippetRange": range(0, 4, 0, 5) }),
            DEFAULT_LSP_REQUEST_TIMEOUT,
        )
        .await
        .into_response();
    assert!(
        result.is_err(),
        "the backend must learn nothing was applied"
    );
}

#[gpui::test]
async fn test_quick_pick_answers_with_the_picked_items(cx: &mut TestAppContext) {
    let (workspace, _editor, server, cx) = open_editor("var x = 1;\n", cx).await;
    let items = json!([
        { "label": "Remove unused variables in file", "kind": 0 },
        { "label": "Remove unused variables in project", "kind": 0 },
    ]);
    let params = json!({ "items": items, "options": { "title": "Select submenu option" } });
    let picker = |cx: &mut VisualTestContext| {
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            workspace.active_modal::<Picker<QuickPick>>(cx)
        })
    };

    let picked = cx.background_spawn(
        server
            .server
            .request::<PickOne>(params.clone(), DEFAULT_LSP_REQUEST_TIMEOUT),
    );
    assert!(picker(cx).is_some());
    cx.dispatch_action(menu::SelectNext);
    cx.dispatch_action(menu::Confirm);
    assert_eq!(picked.await.into_response().unwrap(), items[1]);
    assert!(picker(cx).is_none());

    let dismissed = cx.background_spawn(
        server
            .server
            .request::<PickOne>(params.clone(), DEFAULT_LSP_REQUEST_TIMEOUT),
    );
    assert!(picker(cx).is_some());
    cx.dispatch_action(menu::Cancel);
    assert_eq!(dismissed.await.into_response().unwrap(), Value::Null);

    let many = cx.background_spawn(
        server
            .server
            .request::<PickMany>(params, DEFAULT_LSP_REQUEST_TIMEOUT),
    );
    assert!(picker(cx).is_some());
    cx.dispatch_action(picker::MultiSelectNext);
    cx.dispatch_action(picker::MultiSelectNext);
    assert!(picker(cx).is_some(), "tab only toggles items");
    cx.dispatch_action(menu::Confirm);
    assert_eq!(many.await.into_response().unwrap(), items);
}

fn range(start_line: u32, start: u32, end_line: u32, end: u32) -> Value {
    json!({
        "start": { "line": start_line, "character": start },
        "end": { "line": end_line, "character": end },
    })
}
