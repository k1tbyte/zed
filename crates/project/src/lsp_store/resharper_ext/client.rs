//! What the backend asks of the client while it runs commands and background work.

use anyhow::{Context as _, Result};
use gpui::{App, AsyncApp, EventEmitter, PromptLevel, WeakEntity};
use lsp::{LanguageServer, LanguageServerId};
use serde::{Deserialize, Serialize};
use util::ResultExt as _;

use super::{NoParams, prompt, request};
use crate::{
    LspStore,
    lsp_store::{LanguageServerProgress, ProgressToken},
};

const BACKGROUND_TASKS_TOKEN: &str = "resharper/backgroundTasks";

pub type Reply<T> = async_channel::Sender<T>;

/// Backend requests that need an editor or a picker; the UI layer subscribes to them on the
/// `LspStore`. A sender dropped unanswered fails the edit or cancels the pick.
pub enum UiRequest {
    /// `snippet_range` is in the coordinates of the text after `edits`.
    ApplySnippet {
        edits: Vec<lsp::TextEdit>,
        snippet: String,
        snippet_range: lsp::Range,
        applied: Reply<()>,
    },
    Pick {
        title: Option<String>,
        labels: Vec<String>,
        many: bool,
        /// `None` when the picker is dismissed.
        picked: Reply<Option<Vec<usize>>>,
    },
}

impl EventEmitter<UiRequest> for LspStore {}

enum ApplyChangesWithSnippet {}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApplyChangesWithSnippetParams {
    edits: Vec<lsp::TextEdit>,
    snippet_text: String,
    snippet_range: lsp::Range,
}

impl lsp::request::Request for ApplyChangesWithSnippet {
    type Params = ApplyChangesWithSnippetParams;
    type Result = ();
    const METHOD: &'static str = "resharper/snippets/applyChangesWithSnippet";
}

#[derive(Serialize, Deserialize)]
struct QuickPickParams {
    items: Vec<serde_json::Value>,
    options: Option<QuickPickOptions>,
}

#[derive(Serialize, Deserialize)]
struct QuickPickOptions {
    title: Option<String>,
}

enum PickOne {}

impl lsp::request::Request for PickOne {
    type Params = QuickPickParams;
    type Result = Option<serde_json::Value>;
    const METHOD: &'static str = "resharper/quickPick/pickOne";
}

enum PickMany {}

impl lsp::request::Request for PickMany {
    type Params = QuickPickParams;
    type Result = Option<Vec<serde_json::Value>>;
    const METHOD: &'static str = "resharper/quickPick/pickMany";
}

enum ShowConflicts {}

#[derive(Serialize, Deserialize)]
struct ShowConflictsParams {
    conflicts: Vec<Conflict>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Conflict {
    conflict_text: String,
}

impl lsp::request::Request for ShowConflicts {
    type Params = ShowConflictsParams;
    type Result = ();
    const METHOD: &'static str = "resharper/refactoring/conflicts/show";
}

value_request!(
    AcceptConflicts,
    NoParams,
    "resharper/rename/acceptConflicts"
);
value_request!(
    DiscardConflicts,
    NoParams,
    "resharper/rename/discardConflicts"
);

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProgressParams {
    id: lsp::NumberOrString,
    title: Option<String>,
    task_name: Option<String>,
    current_item_text: Option<String>,
    progress: Option<f64>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BackgroundTasksParams {
    task_progresses: Vec<BackgroundTask>,
}

#[derive(Serialize, Deserialize)]
struct BackgroundTask {
    title: String,
    progress: Option<f64>,
}

macro_rules! notification {
    ($name:ident, $params:ty, $method:literal) => {
        enum $name {}

        impl lsp::notification::Notification for $name {
            type Params = $params;
            const METHOD: &'static str = $method;
        }
    };
}

notification!(
    ProgressStart,
    ProgressParams,
    "resharper/progressIndicator/start"
);
notification!(
    ProgressUpdate,
    ProgressParams,
    "resharper/progressIndicator/update"
);
notification!(
    ProgressStop,
    lsp::NumberOrString,
    "resharper/progressIndicator/stop"
);
notification!(
    BackgroundTasks,
    BackgroundTasksParams,
    "resharper/backgroundTasks/didChangeStatus"
);

/// Frequent notifications that drive VS Code-only UI; unhandled ones are logged with their params.
macro_rules! ignore_notifications {
    ($server:expr, $($method:literal),* $(,)?) => {$({
        notification!(Ignored, serde_json::Value, $method);
        $server.on_notification::<Ignored, _>(|_, _| {}).detach();
    })*};
}

pub(super) fn register(lsp_store: WeakEntity<LspStore>, server: &LanguageServer) {
    let server_id = server.server_id();
    server
        .on_request::<ApplyChangesWithSnippet, _, _>({
            let lsp_store = lsp_store.clone();
            move |params, cx| {
                let (applied, done) = async_channel::bounded(1);
                let emitted = lsp_store.update(cx, |_, cx| {
                    cx.emit(UiRequest::ApplySnippet {
                        edits: params.edits,
                        snippet: params.snippet_text,
                        snippet_range: params.snippet_range,
                        applied,
                    })
                });
                async move {
                    emitted?;
                    done.recv()
                        .await
                        .context("no active editor took the ReSharper changes")
                }
            }
        })
        .detach();
    server
        .on_request::<PickOne, _, _>({
            let lsp_store = lsp_store.clone();
            move |params, cx| {
                let picked = pick(&lsp_store, params, false, cx);
                async move { Ok(picked.await?.and_then(|items| items.into_iter().next())) }
            }
        })
        .detach();
    server
        .on_request::<PickMany, _, _>({
            let lsp_store = lsp_store.clone();
            move |params, cx| pick(&lsp_store, params, true, cx)
        })
        .detach();
    server
        .on_request::<ShowConflicts, _, _>({
            let lsp_store = lsp_store.clone();
            move |params, cx| {
                let lsp_store = lsp_store.clone();
                cx.spawn(async move |cx| {
                    resolve_conflicts(lsp_store, server_id, params.conflicts, cx)
                        .await
                        .log_err()
                })
                .detach();
                async { Ok(()) }
            }
        })
        .detach();

    server
        .on_notification::<ProgressStart, _>({
            let lsp_store = lsp_store.clone();
            move |params, cx| {
                lsp_store
                    .update(cx, |lsp_store, cx| {
                        let work = work(params.title, params.task_name, params.progress, cx);
                        let token = ProgressToken::from_lsp(params.id);
                        lsp_store.on_lsp_work_start(server_id, token, work, cx);
                    })
                    .ok();
            }
        })
        .detach();
    server
        .on_notification::<ProgressUpdate, _>({
            let lsp_store = lsp_store.clone();
            move |params, cx| {
                lsp_store
                    .update(cx, |lsp_store, cx| {
                        let token = ProgressToken::from_lsp(params.id);
                        // Short commands report progress without ever starting an indicator.
                        if is_pending(lsp_store, server_id, &token) {
                            let message = [params.task_name, params.current_item_text]
                                .into_iter()
                                .flatten()
                                .filter(|part| !part.is_empty())
                                .collect::<Vec<_>>()
                                .join(". ");
                            let work = work(None, Some(message), params.progress, cx);
                            lsp_store.on_lsp_work_progress(server_id, token, work, cx);
                        }
                    })
                    .ok();
            }
        })
        .detach();
    server
        .on_notification::<ProgressStop, _>({
            let lsp_store = lsp_store.clone();
            move |id, cx| {
                lsp_store
                    .update(cx, |lsp_store, cx| {
                        end_work(lsp_store, server_id, ProgressToken::from_lsp(id), cx)
                    })
                    .ok();
            }
        })
        .detach();
    server
        .on_notification::<BackgroundTasks, _>(move |params, cx| {
            lsp_store
                .update(cx, |lsp_store, cx| {
                    let token = ProgressToken::String(BACKGROUND_TASKS_TOKEN.into());
                    let tasks = params.task_progresses;
                    let Some(task) = tasks.first() else {
                        return end_work(lsp_store, server_id, token, cx);
                    };
                    let title = match tasks.len() - 1 {
                        0 => task.title.clone(),
                        more => format!("{} (+{more} more)", task.title),
                    };
                    // A start replaces the title, which a progress update cannot.
                    let work = work(Some(title), None, task.progress, cx);
                    lsp_store.on_lsp_work_start(server_id, token, work, cx);
                })
                .ok();
        })
        .detach();

    ignore_notifications!(
        server,
        "resharper/caches/stateChanged",
        "resharper/daemon/stateChanged",
        "resharper/diagnostics/logging/traceCategoriesUpdated",
        "resharper/errorTrap/setShowExceptionsToUser",
        "resharper/nuGet/log",
        "resharper/productivityGuide/actionExecuted",
        "resharper/productivityGuide/renameDone",
        "resharper/refactoring/rename/conflicts/accept/disabled",
        "resharper/region/settings/mapping",
        "resharper/roslyn/enabledChanged",
        "resharper/roslyn/enabledInSettingsChanged",
        "resharper/roslyn/eventFired",
        "resharper/roslyn/readyFired",
        "resharper/roslyn/sourceGeneratorsStatusChanged",
        "resharper/roslyn/statusBarIconChanged",
        "resharper/roslyn/syncStatusChanged",
        "resharper/solution/didOpen",
        "resharper/solutionExplorer/didChangeNodes",
    );
}

/// Resolves to the picked items, or `None` when the picker is dismissed.
fn pick(
    lsp_store: &WeakEntity<LspStore>,
    params: QuickPickParams,
    many: bool,
    cx: &mut AsyncApp,
) -> impl Future<Output = Result<Option<Vec<serde_json::Value>>>> + use<> {
    let (picked, indices) = async_channel::bounded(1);
    let labels = params
        .items
        .iter()
        .map(|item| {
            let label = item.get("label").and_then(|label| label.as_str());
            label.unwrap_or_default().to_string()
        })
        .collect();
    let emitted = lsp_store.update(cx, |_, cx| {
        cx.emit(UiRequest::Pick {
            title: params.options.and_then(|options| options.title),
            labels,
            many,
            picked,
        })
    });
    let items = params.items;
    async move {
        emitted?;
        Ok(indices.recv().await.ok().flatten().map(|indices| {
            indices
                .into_iter()
                .filter_map(|index| items.get(index).cloned())
                .collect()
        }))
    }
}

/// The backend holds a rename back until its conflicts are accepted or discarded.
async fn resolve_conflicts(
    lsp_store: WeakEntity<LspStore>,
    server_id: LanguageServerId,
    conflicts: Vec<Conflict>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let server = lsp_store
        .read_with(cx, |lsp_store, _| {
            lsp_store.language_server_for_id(server_id)
        })?
        .context("ReSharper server is not running")?;
    let names = conflicts
        .iter()
        .map(|conflict| conflict.conflict_text.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let accepted = prompt(
        &lsp_store,
        PromptLevel::Warning,
        &format!("The new name conflicts with {names}"),
        vec!["Rename Anyway".into()],
        cx,
    )
    .await?
    .is_some();
    if accepted {
        request::<AcceptConflicts>(&server, NoParams {}).await
    } else {
        request::<DiscardConflicts>(&server, NoParams {}).await
    }
}

fn work(
    title: Option<String>,
    message: Option<String>,
    fraction: Option<f64>,
    cx: &App,
) -> LanguageServerProgress {
    LanguageServerProgress {
        title,
        message: message.filter(|message| !message.is_empty()),
        percentage: fraction.map(|fraction| (fraction.clamp(0.0, 1.0) * 100.0) as usize),
        is_disk_based_diagnostics_progress: false,
        is_cancellable: false,
        last_update_at: cx.background_executor().now(),
    }
}

fn is_pending(lsp_store: &LspStore, server_id: LanguageServerId, token: &ProgressToken) -> bool {
    lsp_store
        .language_server_statuses
        .get(&server_id)
        .is_some_and(|status| status.pending_work.contains_key(token))
}

fn end_work(
    lsp_store: &mut LspStore,
    server_id: LanguageServerId,
    token: ProgressToken,
    cx: &mut gpui::Context<LspStore>,
) {
    if is_pending(lsp_store, server_id, &token) {
        lsp_store.on_lsp_work_end(server_id, token, cx);
    }
}
