//! What the ReSharper backend asks of the client, handled in `project::lsp_store::resharper_ext`.

use lsp::DEFAULT_LSP_REQUEST_TIMEOUT;
use pretty_assertions::assert_eq;
use serde_json::Value;

use super::*;

macro_rules! backend_notification {
    ($name:ident, $method:literal) => {
        enum $name {}

        impl lsp::notification::Notification for $name {
            type Params = Value;
            const METHOD: &'static str = $method;
        }
    };
}

macro_rules! request {
    ($name:ident, $method:literal) => {
        enum $name {}

        impl lsp::request::Request for $name {
            type Params = Value;
            type Result = Value;
            const METHOD: &'static str = $method;
        }
    };
}

backend_notification!(BackgroundTasks, "resharper/backgroundTasks/didChangeStatus");
backend_notification!(ProgressStart, "resharper/progressIndicator/start");
backend_notification!(ProgressUpdate, "resharper/progressIndicator/update");
backend_notification!(ProgressStop, "resharper/progressIndicator/stop");
request!(ShowConflicts, "resharper/refactoring/conflicts/show");
request!(AcceptConflicts, "resharper/rename/acceptConflicts");
request!(DiscardConflicts, "resharper/rename/discardConflicts");

async fn start_backend(
    capabilities: lsp::ServerCapabilities,
    cx: &mut gpui::TestAppContext,
) -> (Entity<Project>, Entity<Buffer>, lsp::FakeLanguageServer) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/the-root"), json!({ "a.rs": "fn main() {}" }))
        .await;
    fs.insert_tree(
        path!("/sources cache"),
        json!({ "Console.cs": "class Console {}" }),
    )
    .await;
    let project = Project::test(fs, [path!("/the-root").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let mut fake_servers = language_registry.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            name: "resharper",
            capabilities,
            ..FakeLspAdapter::default()
        },
    );
    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/the-root/a.rs"), cx)
        })
        .await
        .unwrap();
    let fake_server = fake_servers.next().await.unwrap();
    cx.executor().run_until_parked();
    (project, buffer, fake_server)
}

/// Title, message and percentage of every piece of pending work.
fn pending_work(
    project: &Entity<Project>,
    cx: &mut gpui::TestAppContext,
) -> Vec<(Option<String>, Option<String>, Option<usize>)> {
    cx.executor().run_until_parked();
    project.read_with(cx, |project, cx| {
        project
            .lsp_store()
            .read(cx)
            .language_server_statuses()
            .flat_map(|(_, status)| status.pending_work.values())
            .map(|work| (work.title.clone(), work.message.clone(), work.percentage))
            .collect()
    })
}

#[gpui::test]
async fn test_backend_progress_shows_as_pending_work(cx: &mut gpui::TestAppContext) {
    let (project, _buffer, backend) = start_backend(Default::default(), cx).await;

    backend.notify::<BackgroundTasks>(json!({ "taskProgresses": [
        { "title": "Loading projects", "progress": 0.5 },
        { "title": "Cache processing", "progress": null },
    ] }));
    assert_eq!(
        pending_work(&project, cx),
        [(Some("Loading projects (+1 more)".into()), None, Some(50))]
    );
    backend.notify::<BackgroundTasks>(json!({ "taskProgresses": [] }));
    assert_eq!(pending_work(&project, cx), []);

    // Quick fixes report progress without starting an indicator.
    let update = json!({ "id": "1", "cancellable": true, "title": "To const", "taskName": "Applying", "currentItemText": "File 1 of 1", "progress": 0.8 });
    backend.notify::<ProgressUpdate>(update.clone());
    backend.notify::<ProgressStop>(json!("1"));
    assert_eq!(pending_work(&project, cx), []);

    backend.notify::<ProgressStart>(json!({ "id": "1", "cancellable": true, "title": "External Sources", "taskName": "Downloading", "currentItemText": null, "progress": 0 }));
    assert_eq!(
        pending_work(&project, cx),
        [(
            Some("External Sources".into()),
            Some("Downloading".into()),
            Some(0)
        )]
    );
    cx.executor().advance_clock(Duration::from_secs(1));
    backend.notify::<ProgressUpdate>(update);
    assert_eq!(
        pending_work(&project, cx),
        [(
            Some("External Sources".into()),
            Some("Applying. File 1 of 1".into()),
            Some(80)
        )]
    );
    backend.notify::<ProgressStop>(json!("1"));
    assert_eq!(pending_work(&project, cx), []);
}

#[gpui::test]
async fn test_rename_conflicts_are_accepted_or_discarded(cx: &mut gpui::TestAppContext) {
    let (project, _buffer, backend) = start_backend(Default::default(), cx).await;
    let (prompts_tx, mut prompts) = futures::channel::mpsc::unbounded();
    cx.update(|cx| {
        cx.subscribe(&project, move |_, event, _| {
            if let project::Event::LanguageServerPrompt(prompt) = event {
                prompts_tx.unbounded_send(prompt.clone()).unwrap();
            }
        })
        .detach();
    });
    let mut accepted =
        backend.set_request_handler::<AcceptConflicts, _, _>(|_, _| async { Ok(json!(true)) });
    let mut discarded =
        backend.set_request_handler::<DiscardConflicts, _, _>(|_, _| async { Ok(Value::Null) });
    let conflicts = json!({ "conflicts": [
        { "documentUri": "file:///the-root/a.rs", "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } }, "groupText": "a.rs", "conflictText": "Calculator.Sub" },
        { "documentUri": "file:///the-root/a.rs", "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } }, "groupText": "a.rs", "conflictText": "calculator.Sub" },
    ] });

    // The backend does not wait for the decision: it ends the rename with no edits.
    let shown = backend.request::<ShowConflicts>(conflicts.clone(), DEFAULT_LSP_REQUEST_TIMEOUT);
    assert_eq!(shown.await.into_response().unwrap(), Value::Null);
    let prompt = prompts.next().await.unwrap();
    assert_eq!(
        prompt.message,
        "The new name conflicts with Calculator.Sub, calculator.Sub"
    );
    prompt.respond(0).await.unwrap();
    accepted.next().await.unwrap();

    let shown = backend.request::<ShowConflicts>(conflicts, DEFAULT_LSP_REQUEST_TIMEOUT);
    shown.await.into_response().unwrap();
    drop(prompts.next().await.unwrap());
    discarded.next().await.unwrap();
}

#[gpui::test]
async fn test_definition_opens_decompiled_sources(cx: &mut gpui::TestAppContext) {
    let capabilities = lsp::ServerCapabilities {
        definition_provider: Some(lsp::OneOf::Left(true)),
        ..lsp::ServerCapabilities::default()
    };
    let (project, buffer, backend) = start_backend(capabilities, cx).await;
    backend.set_request_handler::<lsp::request::GotoDefinition, _, _>(|_, _| async {
        // The scheme the backend gives sources it decompiled to disk.
        let uri = "resharper-decompiled:/sources%20cache/Console.cs";
        Ok(Some(lsp::GotoDefinitionResponse::Scalar(lsp::Location {
            uri: lsp::Uri::from_str(uri).unwrap(),
            range: lsp::Range::new(lsp::Position::new(0, 6), lsp::Position::new(0, 13)),
        })))
    });

    let definitions = project
        .update(cx, |project, cx| project.definitions(&buffer, 3, cx))
        .await
        .unwrap()
        .unwrap();

    let [definition] = definitions.as_slice() else {
        panic!("expected one definition, got {}", definitions.len());
    };
    cx.update(|cx| {
        let target = definition.target.buffer.read(cx);
        let file = target.file().unwrap().as_local().unwrap();
        assert_eq!(
            file.abs_path(cx),
            Path::new(path!("/sources cache/Console.cs"))
        );
        assert_eq!(definition.target.range.to_offset(target), 6..13);
    });
}
