use std::time::Duration;

use anyhow::{Context as _, Result};
use gpui::{App, AsyncApp, Entity, PromptLevel, WeakEntity};
use lsp::{LanguageServer, LanguageServerId};
use serde::{Deserialize, Serialize};
use util::ResultExt as _;

use super::{NoParams, any_server, prompt, request};
use crate::LspStore;

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

value_request!(JbaLogin, NoParams, "resharper/licensing/jba/login");
value_request!(JbaLogout, NoParams, "resharper/licensing/jba/logout");
value_request!(
    UpdateSubscriptions,
    NoParams,
    "resharper/licensing/subscriptions/update"
);
value_request!(
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

pub(super) fn register(lsp_store: WeakEntity<LspStore>, language_server: &LanguageServer) {
    let server_id = language_server.server_id();
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

async fn sign_in_to(server: &LanguageServer) -> Result<()> {
    // The backend opens the browser and answers once the account is authorized.
    server
        .request::<JbaLogin>(NoParams {}, Duration::MAX)
        .await
        .into_response()?;
    request::<UpdateSubscriptions>(server, NoParams {}).await
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
