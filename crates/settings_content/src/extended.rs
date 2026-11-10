use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings_macros::{MergeFrom, with_fallible_options};

/// Settings added by this fork of Zed.
#[with_fallible_options]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ExtendedSettingsContent {
    pub resharper: Option<ResharperSettingsContent>,
}

#[with_fallible_options]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ResharperSettingsContent {
    /// Whether ReSharper is the only C# language server, unless `languages.CSharp.language_servers` says otherwise.
    ///
    /// Default: false
    pub enabled: Option<bool>,
    /// URL of the backend zip archive, `{platform}` expands to e.g. `macos-arm64`.
    pub download_url: Option<String>,
    /// SHA-256 of the archive, checked when set.
    pub sha256: Option<String>,
}
