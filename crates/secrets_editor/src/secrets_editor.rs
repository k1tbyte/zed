mod age_key;
mod environments;
mod passphrase_modal;
mod secret_tree;
mod secrets_group;
mod secrets_view;
mod sops;

use std::path::PathBuf;

use gpui::App;
use settings::{RegisterSetting, Settings};

#[derive(Clone, Debug, Default, RegisterSetting)]
pub struct SopsSettings {
    pub file_patterns: Vec<String>,
    pub age_key_files: Vec<PathBuf>,
}

impl Settings for SopsSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let sops = content.sops.clone().unwrap_or_default();
        Self {
            file_patterns: sops.file_patterns.unwrap_or_default(),
            age_key_files: sops
                .age_key_files
                .unwrap_or_default()
                .iter()
                .map(|path| PathBuf::from(shellexpand::tilde(path).as_ref()))
                .collect(),
        }
    }
}

pub fn init(cx: &mut App) {
    age_key::init(cx);
    workspace::register_project_item::<secrets_view::SecretsView>(cx);
}
