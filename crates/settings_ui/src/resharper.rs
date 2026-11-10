// Fork: ReSharper options on the C# language page, stored in `language_servers` and `lsp.resharper.settings`.
use gpui::ReadGlobal as _;
use project::lsp_store::resharper_ext::RESHARPER_SERVER_NAME;
use serde_json::{Map, Value, json};
use settings::{ConfiguredLanguageServer, SettingsContent, SettingsStore};

use crate::{
    PROJECT, SettingField, SettingItem, SettingsFieldMetadata, SettingsPageItem, USER,
    active_language,
};

const CSHARP: &str = "CSharp";
const EMPTY: String = String::new();
// Without a default the debug build shows "NO DEFAULT" instead of the control. As a side effect
// the project scope shows only its own value, not the one inherited from the user scope.
const EMPTY_STRING: Option<&String> = Some(&EMPTY);

/// Empty unless the C# page is open.
pub(crate) fn csharp_section() -> Box<[SettingsPageItem]> {
    if active_language().as_deref() != Some(CSHARP) {
        return Box::new([]);
    }
    Box::new([
        SettingsPageItem::SectionHeader("ReSharper"),
        SettingsPageItem::SettingItem(SettingItem {
            title: "Use ReSharper",
            description: "Use ReSharper as the only C# language server. Sign in with the `resharper: sign in` action.",
            field: Box::new(SettingField {
                organization_override: None,
                json_path: Some("languages.$(language).language_servers"),
                pick: |content| {
                    language_servers(content).map(|servers| {
                        if uses_resharper(servers) {
                            &true
                        } else {
                            &false
                        }
                    })
                },
                write: |content, value, cx| {
                    let default_servers =
                        language_servers(SettingsStore::global(cx).raw_default_settings());
                    set_resharper(content, value.unwrap_or(false), default_servers.cloned())
                },
            }),
            metadata: None,
            files: USER | PROJECT,
        }),
        SettingsPageItem::SettingItem(SettingItem {
            title: "ReSharper Download URL",
            description: "URL of the ReSharper backend zip archive. `{platform}` expands to e.g. `macos-arm64`.",
            field: Box::new(SettingField {
                organization_override: None,
                json_path: Some("lsp.resharper.settings.download_url"),
                pick: |content| option(content, "download_url").or(EMPTY_STRING),
                write: |content, value, _| set_option(content, "download_url", value),
            }),
            metadata: Some(Box::new(SettingsFieldMetadata {
                placeholder: Some("https://.../Backend.<version>.{platform}.zip"),
                ..Default::default()
            })),
            files: USER | PROJECT,
        }),
        SettingsPageItem::SettingItem(SettingItem {
            title: "ReSharper Archive SHA-256",
            description: "Checksum the downloaded archive must match. Not checked when empty.",
            field: Box::new(SettingField {
                organization_override: None,
                json_path: Some("lsp.resharper.settings.sha256"),
                pick: |content| option(content, "sha256").or(EMPTY_STRING),
                write: |content, value, _| set_option(content, "sha256", value),
            }),
            metadata: None,
            files: USER | PROJECT,
        }),
    ])
}

fn language_servers(content: &SettingsContent) -> Option<&Vec<ConfiguredLanguageServer>> {
    content
        .project
        .all_languages
        .languages
        .0
        .get(CSHARP)?
        .language_servers
        .as_ref()
}

fn uses_resharper(servers: &[ConfiguredLanguageServer]) -> bool {
    servers
        .iter()
        .any(|server| is_resharper(&server.name) && !server.disabled)
}

fn is_resharper(name: &str) -> bool {
    name == &*RESHARPER_SERVER_NAME.0
}

/// Turning it off keeps other hand-written entries. An empty list falls back to the default servers
/// instead of `None`, which would inherit ReSharper from a parent scope.
fn set_resharper(
    content: &mut SettingsContent,
    enabled: bool,
    default_servers: Option<Vec<ConfiguredLanguageServer>>,
) {
    let language = content
        .project
        .all_languages
        .languages
        .0
        .entry(CSHARP.into())
        .or_default();
    if enabled {
        language.language_servers = Some(vec![ConfiguredLanguageServer::new(
            RESHARPER_SERVER_NAME.0.as_ref(),
        )]);
        return;
    }
    let mut servers = language.language_servers.take().unwrap_or_default();
    servers.retain(|server| !is_resharper(&server.name));
    if servers.is_empty() {
        servers = default_servers.unwrap_or_default();
    }
    language.language_servers = Some(servers);
}

fn option<'a>(content: &'a SettingsContent, key: &str) -> Option<&'a String> {
    let lsp = content
        .project
        .lsp
        .0
        .get(RESHARPER_SERVER_NAME.0.as_ref())?;
    match lsp.settings.as_ref()?.get(key)? {
        Value::String(value) => Some(value),
        _ => None,
    }
}

fn set_option(content: &mut SettingsContent, key: &str, value: Option<String>) {
    let value = value.filter(|value| !value.is_empty());
    let Some(options) = options_mut(content, value.is_some()) else {
        return;
    };
    match value {
        Some(value) => options.insert(key.to_string(), Value::String(value)),
        None => options.remove(key),
    };
}

/// Clearing a value must not create `lsp.resharper` just to remove a key from it.
fn options_mut(content: &mut SettingsContent, create: bool) -> Option<&mut Map<String, Value>> {
    let server_name = RESHARPER_SERVER_NAME;
    let name: &str = &server_name.0;
    let lsp = &mut content.project.lsp.0;
    if create && !lsp.contains_key(name) {
        lsp.insert(name.into(), serde_json::from_value(json!({})).ok()?);
    }
    let server = lsp.get_mut(name)?;
    if create && !matches!(server.settings, Some(Value::Object(_))) {
        server.settings = Some(json!({}));
    }
    server.settings.as_mut()?.as_object_mut()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_servers() -> Option<Vec<ConfiguredLanguageServer>> {
        Some(vec![
            ConfiguredLanguageServer::new("roslyn"),
            ConfiguredLanguageServer::new("..."),
        ])
    }

    #[test]
    fn test_unset_language_servers_are_not_picked() {
        let content = SettingsContent::default();
        assert_eq!(language_servers(&content), None);
    }

    #[test]
    fn test_use_resharper_toggle() {
        let mut content = SettingsContent::default();

        set_resharper(&mut content, true, default_servers());
        assert!(uses_resharper(language_servers(&content).unwrap()));

        set_resharper(&mut content, false, default_servers());
        assert!(!uses_resharper(language_servers(&content).unwrap()));
    }

    #[test]
    fn test_turning_resharper_off_keeps_other_servers() {
        let mut content = SettingsContent::default();
        content
            .project
            .all_languages
            .languages
            .0
            .entry(CSHARP.into())
            .or_default()
            .language_servers = Some(vec![
            ConfiguredLanguageServer::new("resharper"),
            ConfiguredLanguageServer::new("roslyn"),
        ]);

        set_resharper(&mut content, false, default_servers());
        assert_eq!(
            language_servers(&content),
            Some(&vec![ConfiguredLanguageServer::new("roslyn")])
        );
    }

    #[test]
    fn test_turning_resharper_off_writes_the_default_servers() {
        let mut content = SettingsContent::default();
        set_resharper(&mut content, false, default_servers());
        assert_eq!(language_servers(&content), default_servers().as_ref());
    }

    #[test]
    fn test_download_options_round_trip() {
        let mut content = SettingsContent::default();
        assert_eq!(option(&content, "download_url"), None);

        set_option(
            &mut content,
            "download_url",
            Some("https://x/{platform}.zip".into()),
        );
        set_option(&mut content, "sha256", Some("abc".into()));
        assert_eq!(
            option(&content, "download_url").map(String::as_str),
            Some("https://x/{platform}.zip")
        );

        set_option(&mut content, "download_url", Some(String::new()));
        assert_eq!(option(&content, "download_url"), None);
        assert_eq!(option(&content, "sha256").map(String::as_str), Some("abc"));
    }

    #[test]
    fn test_option_is_written_over_null_settings() {
        let mut content = SettingsContent::default();
        content.project.lsp.0.insert(
            RESHARPER_SERVER_NAME.0.as_ref().into(),
            serde_json::from_value(json!({ "settings": null })).unwrap(),
        );

        set_option(&mut content, "sha256", Some("abc".into()));
        assert_eq!(option(&content, "sha256").map(String::as_str), Some("abc"));
    }

    #[test]
    fn test_clearing_an_option_does_not_create_the_server_entry() {
        let mut content = SettingsContent::default();
        set_option(&mut content, "sha256", None);
        assert!(content.project.lsp.0.is_empty());
    }
}
