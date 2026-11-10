use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context as _, Result};
use futures::StreamExt as _;
use gpui::{App, AsyncApp, Entity, PromptLevel, WeakEntity};
use lsp::{
    DEFAULT_LSP_REQUEST_TIMEOUT, LanguageServer, LanguageServerId, LanguageServerName,
    MessageActionItem, Uri,
};
use serde::{Deserialize, Serialize};
use util::ResultExt as _;

use crate::{LanguageServerPromptRequest, LspStore, LspStoreEvent, lsp_store::LanguageServerState};

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

enum LicensingState {}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LicensingStateParams {
    is_licensed: bool,
    jba_licensing_state: Option<JbaLicensingState>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JbaLicensingState {
    authentication_status: String,
    #[serde(default)]
    available_subscriptions: Vec<Subscription>,
    selected_subscription: Option<Subscription>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Subscription {
    asset_id: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    is_valid: bool,
}

impl lsp::notification::Notification for LicensingState {
    type Params = LicensingStateParams;
    const METHOD: &'static str = "resharper/licensing/state";
}

#[derive(Serialize, Deserialize)]
struct NoParams {}

macro_rules! licensing_request {
    ($name:ident, $params:ty, $method:literal) => {
        enum $name {}

        impl lsp::request::Request for $name {
            type Params = $params;
            type Result = serde_json::Value;
            const METHOD: &'static str = $method;
        }
    };
}

licensing_request!(JbaLogin, NoParams, "resharper/licensing/jba/login");
licensing_request!(JbaLogout, NoParams, "resharper/licensing/jba/logout");
licensing_request!(
    UpdateSubscriptions,
    NoParams,
    "resharper/licensing/subscriptions/update"
);
licensing_request!(
    UseSubscription,
    UseSubscriptionParams,
    "resharper/licensing/subscriptions/use"
);

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UseSubscriptionParams {
    asset_id: String,
}

#[derive(Clone, Debug, PartialEq)]
enum LicensingStep {
    UseSubscription(String),
    ChooseSubscription(Vec<Subscription>),
    SuggestSignIn,
}

/// A signed-in account still needs one of its subscriptions picked before it licenses anything.
fn licensing_step(state: &LicensingStateParams) -> Option<LicensingStep> {
    if let Some(jba) = &state.jba_licensing_state
        && jba.authentication_status == "Authenticated"
        && !jba
            .selected_subscription
            .as_ref()
            .is_some_and(|subscription| subscription.is_valid)
    {
        let mut valid: Vec<Subscription> = jba
            .available_subscriptions
            .iter()
            .filter(|subscription| subscription.is_valid)
            .cloned()
            .collect();
        match valid.len() {
            0 => {}
            1 => return Some(LicensingStep::UseSubscription(valid.remove(0).asset_id)),
            _ => return Some(LicensingStep::ChooseSubscription(valid)),
        }
    }
    (!state.is_licensed).then_some(LicensingStep::SuggestSignIn)
}

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
                        open_solution(lsp_store, server_id, cx).await.log_err()
                    })
                    .detach();
                }
                async { Ok(()) }
            }
        })
        .detach();

    // The backend re-sends the state on every change, so a step is taken only when it changes.
    let mut last_step = None;
    language_server
        .on_notification::<LicensingState, _>(move |state, cx| {
            let step = licensing_step(&state);
            if step == last_step {
                return;
            }
            last_step = step.clone();
            if let Some(step) = step {
                let lsp_store = lsp_store.clone();
                cx.spawn(async move |cx| {
                    take_licensing_step(lsp_store, server_id, step, cx)
                        .await
                        .log_err()
                })
                .detach();
            }
        })
        .detach();
}

async fn take_licensing_step(
    lsp_store: WeakEntity<LspStore>,
    server_id: LanguageServerId,
    step: LicensingStep,
    cx: &mut AsyncApp,
) -> Result<()> {
    let server = lsp_store
        .read_with(cx, |lsp_store, _| {
            lsp_store.language_server_for_id(server_id)
        })?
        .context("ReSharper server is not running")?;
    let asset_id = match step {
        LicensingStep::UseSubscription(asset_id) => asset_id,
        LicensingStep::ChooseSubscription(subscriptions) => {
            let titles = subscriptions
                .iter()
                .map(|subscription| subscription.display_name.clone())
                .collect();
            let Some(index) = prompt(
                &lsp_store,
                PromptLevel::Info,
                "Choose the JetBrains subscription that licenses ReSharper",
                titles,
                cx,
            )
            .await?
            else {
                return Ok(());
            };
            subscriptions[index].asset_id.clone()
        }
        LicensingStep::SuggestSignIn => {
            let signs_in = prompt(
                &lsp_store,
                PromptLevel::Warning,
                "ReSharper is not licensed. Sign in with a JetBrains Account that has a license?",
                vec!["Sign In".into()],
                cx,
            )
            .await?
            .is_some();
            return if signs_in {
                sign_in_to(&server).await
            } else {
                Ok(())
            };
        }
    };
    request::<UseSubscription>(&server, UseSubscriptionParams { asset_id }).await
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

async fn sign_in_to(server: &LanguageServer) -> Result<()> {
    // The backend opens the browser and answers once the account is authorized.
    server
        .request::<JbaLogin>(NoParams {}, Duration::MAX)
        .await
        .into_response()?;
    request::<UpdateSubscriptions>(server, NoParams {}).await
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

pub fn sign_in(lsp_store: &Entity<LspStore>, cx: &App) -> impl Future<Output = Result<()>> + use<> {
    let server = any_server(lsp_store, cx);
    async move { sign_in_to(&*server?).await }
}

pub fn sign_out(
    lsp_store: &Entity<LspStore>,
    cx: &App,
) -> impl Future<Output = Result<()>> + use<> {
    let server = any_server(lsp_store, cx);
    async move { request::<JbaLogout>(&*server?, NoParams {}).await }
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
    let solution = SOLUTION_EXTENSIONS
        .iter()
        .find_map(|extension| {
            candidates
                .iter()
                .find(|path| path.extension().is_some_and(|found| found == *extension))
        })
        .with_context(|| format!("no .slnx, .sln or .csproj found in {root:?}"))?;
    let solution_or_project_file_uri = Uri::from_file_path(solution)
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

#[cfg(test)]
mod tests {
    use super::{LicensingStateParams, LicensingStep, Subscription, licensing_step};
    use serde_json::json;

    fn step(is_licensed: bool, jba: serde_json::Value) -> Option<LicensingStep> {
        let state: LicensingStateParams = serde_json::from_value(json!({
            "jbaLicensingState": jba,
            "selectedActivationCode": null,
            "licenseServerFlowState": null,
            "trialInfo": null,
            "isLicensed": is_licensed,
        }))
        .expect("licensing state");
        licensing_step(&state)
    }

    fn subscription(asset_id: &str, is_valid: bool) -> serde_json::Value {
        json!({ "assetId": asset_id, "licenseId": "L", "displayName": asset_id, "isValid": is_valid })
    }

    fn authenticated(
        available: Vec<serde_json::Value>,
        selected: serde_json::Value,
    ) -> serde_json::Value {
        json!({
            "accountInfo": { "email": "user@example.com" },
            "authenticationStatus": "Authenticated",
            "availableSubscriptions": available,
            "selectedSubscription": selected,
            "isLicencedByOnlyNonCommercial": false,
        })
    }

    #[test]
    fn test_trial_needs_nothing() {
        let jba = json!({
            "accountInfo": null,
            "authenticationStatus": "NotAuthenticated",
            "availableSubscriptions": [],
            "selectedSubscription": null,
            "isLicencedByOnlyNonCommercial": false,
        });
        assert_eq!(step(true, jba.clone()), None);
        assert_eq!(step(false, jba), Some(LicensingStep::SuggestSignIn));
    }

    #[test]
    fn test_signed_in_account_picks_its_subscription() {
        let only = authenticated(
            vec![subscription("A", true), subscription("B", false)],
            json!(null),
        );
        assert_eq!(
            step(false, only),
            Some(LicensingStep::UseSubscription("A".into()))
        );

        let several = authenticated(
            vec![
                subscription("A", true),
                subscription("B", false),
                subscription("C", true),
            ],
            json!(null),
        );
        let Some(LicensingStep::ChooseSubscription(choices)) = step(false, several) else {
            panic!("expected a choice");
        };
        let ids: Vec<&str> = choices
            .iter()
            .map(|choice: &Subscription| choice.asset_id.as_str())
            .collect();
        assert_eq!(ids, ["A", "C"]);

        let selected = authenticated(vec![subscription("A", true)], subscription("A", true));
        assert_eq!(step(true, selected), None);
    }
}
