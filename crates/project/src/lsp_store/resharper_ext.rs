use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context as _, Result};
use futures::StreamExt as _;
use gpui::{App, AsyncApp, Entity, PromptLevel, WeakEntity};
use lsp::{
    DEFAULT_LSP_REQUEST_TIMEOUT, LanguageServer, LanguageServerId, LanguageServerName,
    MessageActionItem, Uri,
};
use serde::{Deserialize, Serialize};

use crate::{LanguageServerPromptRequest, LspStore, LspStoreEvent, lsp_store::LanguageServerState};

/// A request whose answer is not inspected.
macro_rules! value_request {
    ($name:ident, $params:ty, $method:literal) => {
        enum $name {}

        impl lsp::request::Request for $name {
            type Params = $params;
            type Result = serde_json::Value;
            const METHOD: &'static str = $method;
        }
    };
}

mod client;
mod licensing;

pub use client::{Reply, UiRequest};
pub use licensing::{sign_in, sign_out};

pub const RESHARPER_SERVER_NAME: LanguageServerName = LanguageServerName::new_static("resharper");
const SOLUTION_CAPABILITY: &str = "resharper/solution";
/// Preferred first: a solution covers more projects than a single project file.
const SOLUTION_EXTENSIONS: [&str; 3] = ["slnx", "sln", "csproj"];

/// ReSharper's own capability registration; textDocument/* features stay unregistered until
/// the client answers it and opens a solution.
enum CapabilityRegister {}

#[derive(Serialize, Deserialize)]
struct CapabilityRegisterParams {
    registrations: Vec<CapabilityRegistration>,
}

#[derive(Serialize, Deserialize)]
struct CapabilityRegistration {
    name: String,
}

impl lsp::request::Request for CapabilityRegister {
    type Params = CapabilityRegisterParams;
    type Result = ();
    const METHOD: &'static str = "resharper/capability/register";
}

enum SolutionOpen {}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SolutionOpenParams {
    solution_or_project_file_uri: Uri,
}

impl lsp::request::Request for SolutionOpen {
    type Params = SolutionOpenParams;
    type Result = ();
    const METHOD: &'static str = "resharper/solution/open";
}

#[derive(Serialize, Deserialize)]
struct NoParams {}

pub fn register_requests(lsp_store: WeakEntity<LspStore>, language_server: &LanguageServer) {
    if language_server.name() != RESHARPER_SERVER_NAME {
        return;
    }
    let server_id = language_server.server_id();
    language_server
        .on_request::<CapabilityRegister, _, _>({
            let lsp_store = lsp_store.clone();
            move |params, cx| {
                if params
                    .registrations
                    .iter()
                    .any(|registration| registration.name == SOLUTION_CAPABILITY)
                {
                    let lsp_store = lsp_store.clone();
                    cx.spawn(async move |cx| {
                        // Without a solution the backend stays inert, so the user has to see why.
                        if let Err(error) = open_solution(lsp_store.clone(), server_id, cx).await {
                            log::error!("ReSharper: {error:#}");
                            lsp_store
                                .update(cx, |_, cx| {
                                    cx.emit(LspStoreEvent::Notification(format!(
                                        "ReSharper: {error:#}"
                                    )))
                                })
                                .ok();
                        }
                    })
                    .detach();
                }
                async { Ok(()) }
            }
        })
        .detach();
    licensing::register(lsp_store.clone(), language_server);
    client::register(lsp_store, language_server);
}

/// Resolves to the index of the chosen action, or `None` when the prompt is dismissed.
async fn prompt(
    lsp_store: &WeakEntity<LspStore>,
    level: PromptLevel,
    message: &str,
    actions: Vec<String>,
    cx: &mut AsyncApp,
) -> Result<Option<usize>> {
    let (tx, rx) = async_channel::bounded::<MessageActionItem>(1);
    let actions: Vec<MessageActionItem> = actions
        .into_iter()
        .map(|title| MessageActionItem {
            title,
            properties: Default::default(),
        })
        .collect();
    let request = LanguageServerPromptRequest::new(
        level,
        message.to_string(),
        actions.clone(),
        RESHARPER_SERVER_NAME.to_string(),
        tx,
    );
    lsp_store.update(cx, |_, cx| {
        cx.emit(LspStoreEvent::LanguageServerPrompt(request))
    })?;
    Ok(rx
        .recv()
        .await
        .ok()
        .and_then(|chosen| actions.iter().position(|action| *action == chosen)))
}

async fn request<R>(server: &LanguageServer, params: R::Params) -> Result<()>
where
    R: lsp::request::Request<Result = serde_json::Value>,
{
    server
        .request::<R>(params, DEFAULT_LSP_REQUEST_TIMEOUT)
        .await
        .into_response()
        .map(drop)
}

/// Licensing is per machine, so any running ReSharper server will do.
fn any_server(lsp_store: &Entity<LspStore>, cx: &App) -> Result<Arc<LanguageServer>> {
    lsp_store
        .read(cx)
        .as_local()
        .into_iter()
        .flat_map(|local| local.language_servers.values())
        .find_map(|state| match state {
            LanguageServerState::Running { server, .. }
                if server.name() == RESHARPER_SERVER_NAME =>
            {
                Some(server.clone())
            }
            _ => None,
        })
        .context("ReSharper is not running in this project")
}

/// Zed opens buffers in a server right away, but ReSharper registers its `didOpen` handler only
/// after a solution is loaded and drops earlier notifications, so they are sent again.
pub(super) fn reopen_buffers(lsp_store: &LspStore, server: &LanguageServer, cx: &App) {
    if server.name() != RESHARPER_SERVER_NAME {
        return;
    }
    let server_id = server.server_id();
    let Some(local) = lsp_store.as_local() else {
        return;
    };
    let Some(LanguageServerState::Running { adapter, .. }) = local.language_servers.get(&server_id)
    else {
        return;
    };
    for handle in lsp_store.buffer_store.read(cx).buffers() {
        let buffer = handle.read(cx);
        let Some(snapshot) = local
            .buffer_snapshots
            .get(&buffer.remote_id())
            .and_then(|servers| servers.get(&server_id)?.last())
        else {
            continue;
        };
        let (Some(file), Some(language)) = (
            buffer.file().and_then(|file| file.as_local()),
            buffer.language(),
        ) else {
            continue;
        };
        let Ok(uri) = Uri::from_file_path(file.abs_path(cx)) else {
            continue;
        };
        server.register_buffer(
            uri,
            adapter.language_id(&language.name()),
            snapshot.version,
            snapshot.snapshot.text_with_line_endings(),
        );
    }
}

/// The shallowest solution or project file below `root`, for repositories that keep it in a
/// folder such as `src`. A solution wins over a project at the same depth.
async fn nested_solution(
    lsp_store: &WeakEntity<LspStore>,
    root: &Path,
    cx: &mut AsyncApp,
) -> Result<Option<PathBuf>> {
    let worktree = lsp_store.read_with(cx, |lsp_store, cx| {
        lsp_store.worktree_store.read(cx).find_worktree(root, cx)
    })?;
    let Some((worktree, root_path)) = worktree else {
        return Ok(None);
    };
    let scan = worktree.read_with(cx, |worktree, _| {
        worktree.as_local().map(|local| local.scan_complete())
    });
    if let Some(scan) = scan {
        scan.await;
    }
    let snapshot = worktree.read_with(cx, |worktree, _| worktree.snapshot());
    Ok(snapshot
        .files(false, 0)
        .filter(|entry| entry.path.starts_with(&root_path))
        .filter_map(|entry| {
            let extension = entry.path.extension()?;
            let rank = SOLUTION_EXTENSIONS
                .iter()
                .position(|known| *known == extension)?;
            Some(((entry.path.components().count(), rank), &entry.path))
        })
        .min_by_key(|(key, _)| *key)
        .map(|(_, path)| snapshot.absolutize(path)))
}

async fn open_solution(
    lsp_store: WeakEntity<LspStore>,
    server_id: LanguageServerId,
    cx: &mut AsyncApp,
) -> Result<()> {
    let (server, fs) = lsp_store.read_with(cx, |lsp_store, _| {
        (
            lsp_store.language_server_for_id(server_id),
            lsp_store.as_local().map(|local| local.fs.clone()),
        )
    })?;
    let server = server.context("ReSharper server is not running")?;
    let fs = fs.context("ReSharper runs only in local projects")?;
    let root = server
        .root_uri()
        .to_file_path()
        .ok()
        .context("ReSharper root is not a file path")?;

    let mut candidates: Vec<PathBuf> = fs
        .read_dir(&root)
        .await?
        .filter_map(|entry| async { entry.ok() })
        .collect()
        .await;
    candidates.sort();
    let in_root = SOLUTION_EXTENSIONS.iter().find_map(|extension| {
        candidates
            .iter()
            .find(|path| path.extension().is_some_and(|found| found == *extension))
    });
    let solution = match in_root {
        Some(solution) => solution.clone(),
        None => nested_solution(&lsp_store, &root, cx)
            .await?
            .with_context(|| format!("no .slnx, .sln or .csproj found under {root:?}"))?,
    };
    let solution_or_project_file_uri = Uri::from_file_path(&solution)
        .ok()
        .with_context(|| format!("{solution:?} is not a valid URI"))?;

    log::info!("ReSharper: opening {solution:?}");
    // Loading a large solution takes minutes, so the request is not bounded by the LSP timeout.
    server
        .request::<SolutionOpen>(
            SolutionOpenParams {
                solution_or_project_file_uri,
            },
            Duration::MAX,
        )
        .await
        .into_response()
}
