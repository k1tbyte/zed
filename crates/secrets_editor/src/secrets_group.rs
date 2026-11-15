use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Result, bail};
use collections::{HashMap, IndexMap};
use fs::Fs;
use gpui::{AppContext as _, Context, SharedString, Task};
use project::{ProjectEntryId, ProjectPath};
use serde_json::Value;
use util::ResultExt as _;

use crate::{
    environments::SecretsFormat,
    secret_tree::{KeyPath, age_recipients, display_value, edited_value, encrypted_keys, leaves},
    sops::{self, Change},
};

pub(crate) enum EnvironmentState {
    Locked {
        keys: Vec<KeyPath>,
        error: Option<SharedString>,
    },
    Unlocked {
        values: IndexMap<KeyPath, Value>,
    },
}

pub(crate) struct EnvironmentFile {
    pub name: SharedString,
    pub abs_path: PathBuf,
    pub format: SecretsFormat,
    pub state: EnvironmentState,
    pub recipients: Vec<String>,
}

impl EnvironmentFile {
    fn keys(&self) -> Box<dyn Iterator<Item = &KeyPath> + '_> {
        match &self.state {
            EnvironmentState::Locked { keys, .. } => Box::new(keys.iter()),
            EnvironmentState::Unlocked { values } => Box::new(values.keys()),
        }
    }

    fn value(&self, key: &KeyPath) -> Option<&Value> {
        match &self.state {
            EnvironmentState::Unlocked { values } => values.get(key),
            EnvironmentState::Locked { .. } => None,
        }
    }

    pub fn is_unlocked(&self) -> bool {
        matches!(self.state, EnvironmentState::Unlocked { .. })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PendingEdit {
    Set(Value),
    Unset,
}

#[derive(Debug, PartialEq)]
pub(crate) enum Cell {
    Locked,
    Missing,
    Value { text: String, edited: bool },
    Removed,
}

pub(crate) struct SecretsGroup {
    title: SharedString,
    opened: ProjectPath,
    entry_id: Option<ProjectEntryId>,
    environments: Vec<EnvironmentFile>,
    edits: HashMap<(usize, KeyPath), PendingEdit>,
    added_keys: Vec<KeyPath>,
    saving: bool,
}

impl SecretsGroup {
    pub fn new(
        title: SharedString,
        opened: ProjectPath,
        entry_id: Option<ProjectEntryId>,
        environments: Vec<EnvironmentFile>,
    ) -> Self {
        Self {
            title,
            opened,
            entry_id,
            environments,
            edits: HashMap::default(),
            added_keys: Vec::new(),
            saving: false,
        }
    }

    pub fn title(&self) -> &SharedString {
        &self.title
    }

    pub fn file_name(&self) -> SharedString {
        self.title
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_string()
            .into()
    }

    pub fn opened_path(&self) -> &ProjectPath {
        &self.opened
    }

    pub fn environments(&self) -> &[EnvironmentFile] {
        &self.environments
    }

    pub fn is_dirty(&self) -> bool {
        !self.edits.is_empty()
    }

    pub fn any_locked(&self) -> bool {
        self.environments.iter().any(|file| !file.is_unlocked())
    }

    pub fn any_unlocked(&self) -> bool {
        self.environments.iter().any(EnvironmentFile::is_unlocked)
    }

    pub fn first_unlocked(&self) -> Option<usize> {
        self.environments
            .iter()
            .position(EnvironmentFile::is_unlocked)
    }

    pub fn rows(&self) -> Vec<KeyPath> {
        let mut rows: Vec<KeyPath> = Vec::new();
        let mut seen = collections::HashSet::default();
        let known = self.environments.iter().flat_map(|file| file.keys());
        for key in known.chain(&self.added_keys) {
            if seen.insert(key) {
                rows.push(key.clone());
            }
        }
        rows
    }

    pub fn cell(&self, environment_index: usize, key: &KeyPath) -> Cell {
        let Some(file) = self.environments.get(environment_index) else {
            return Cell::Missing;
        };
        if !file.is_unlocked() {
            return if file.keys().any(|known| known == key) {
                Cell::Locked
            } else {
                Cell::Missing
            };
        }
        match self.edits.get(&(environment_index, key.clone())) {
            Some(PendingEdit::Set(value)) => Cell::Value {
                text: display_value(value),
                edited: true,
            },
            Some(PendingEdit::Unset) => Cell::Removed,
            None => match file.value(key) {
                Some(value) => Cell::Value {
                    text: display_value(value),
                    edited: false,
                },
                None => Cell::Missing,
            },
        }
    }

    pub fn set_value(&mut self, environment_index: usize, key: KeyPath, text: &str) {
        let Some(file) = self
            .environments
            .get(environment_index)
            .filter(|file| file.is_unlocked())
        else {
            return;
        };
        let original = file.value(&key);
        let edited = edited_value(original, text);
        let edit_key = (environment_index, key);
        // During a save the file values are stale, so keep the edit and let decrypt reconcile it.
        if original == Some(&edited) && !self.saving {
            self.edits.remove(&edit_key);
        } else {
            self.edits.insert(edit_key, PendingEdit::Set(edited));
        }
    }

    /// Array elements are not removable: sops shifts later indices, breaking queued changes.
    pub fn unset(&mut self, environment_index: usize, key: KeyPath) {
        if key.has_index() {
            return;
        }
        let Some(file) = self
            .environments
            .get(environment_index)
            .filter(|file| file.is_unlocked())
        else {
            return;
        };
        let exists = file.value(&key).is_some();
        let edit_key = (environment_index, key);
        if exists || self.saving {
            self.edits.insert(edit_key, PendingEdit::Unset);
        } else {
            self.edits.remove(&edit_key);
        }
    }

    pub fn remove_key(&mut self, key: &KeyPath) {
        for environment_index in 0..self.environments.len() {
            self.unset(environment_index, key.clone());
        }
        self.added_keys.retain(|added| added != key);
    }

    pub fn add_key(&mut self, text: &str) -> Result<KeyPath> {
        let format = self.environments.first().map(|file| file.format);
        let Some(key) = format.and_then(|format| KeyPath::parse(text, format)) else {
            bail!("invalid key name");
        };
        // `sops set` on an ancestor or descendant path would replace the existing subtree.
        if let Some(existing) = self.rows().into_iter().find(|row| row.overlaps(&key)) {
            bail!("{key} conflicts with existing key {existing}");
        }
        self.added_keys.push(key.clone());
        Ok(key)
    }

    fn has_key(&self, environment_index: usize, file: &EnvironmentFile, key: &KeyPath) -> bool {
        match self.edits.get(&(environment_index, key.clone())) {
            Some(PendingEdit::Set(_)) => true,
            Some(PendingEdit::Unset) => false,
            None => file.keys().any(|known| known == key),
        }
    }

    pub fn missing(&self) -> Vec<(SharedString, Vec<KeyPath>)> {
        if self.environments.len() < 2 {
            return Vec::new();
        }
        // A key being removed from every environment is not a parity gap.
        let rows: Vec<KeyPath> = self
            .rows()
            .into_iter()
            .filter(|key| {
                self.environments
                    .iter()
                    .enumerate()
                    .any(|(index, file)| self.has_key(index, file, key))
            })
            .collect();
        self.environments
            .iter()
            .enumerate()
            .filter_map(|(index, file)| {
                let absent: Vec<KeyPath> = rows
                    .iter()
                    .filter(|key| !self.has_key(index, file, key))
                    .cloned()
                    .collect();
                (!absent.is_empty()).then(|| (file.name.clone(), absent))
            })
            .collect()
    }

    pub fn changes(&self) -> Vec<Vec<Change>> {
        let rows = self.rows();
        (0..self.environments.len())
            .map(|environment_index| {
                rows.iter()
                    .filter_map(
                        |key| match self.edits.get(&(environment_index, key.clone()))? {
                            PendingEdit::Set(value) => {
                                Some(Change::Set(key.clone(), value.clone()))
                            }
                            PendingEdit::Unset => Some(Change::Unset(key.clone())),
                        },
                    )
                    .collect()
            })
            .collect()
    }

    fn finish_decrypt(&mut self, environment_index: usize, result: Result<Value>) {
        let Some(file) = self.environments.get_mut(environment_index) else {
            return;
        };
        file.state = match result {
            Ok(document) => {
                let values: IndexMap<KeyPath, Value> = leaves(&document).into_iter().collect();
                // Drop edits already on disk, so saves made partly or during editing reconcile.
                self.edits.retain(|(index, key), edit| {
                    *index != environment_index
                        || match edit {
                            PendingEdit::Set(value) => values.get(key) != Some(value),
                            PendingEdit::Unset => values.contains_key(key),
                        }
                });
                self.added_keys.retain(|key| !values.contains_key(key));
                EnvironmentState::Unlocked { values }
            }
            Err(error) => {
                log::error!("failed to decrypt {}: {error:#}", file.abs_path.display());
                EnvironmentState::Locked {
                    keys: file.keys().cloned().collect(),
                    error: Some(format!("{error:#}").into()),
                }
            }
        };
    }

    pub fn decrypt(
        &mut self,
        environment: HashMap<String, String>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let tasks: Vec<_> = self
            .environments
            .iter()
            .map(|file| {
                let environment = environment.clone();
                let abs_path = file.abs_path.clone();
                let format = file.format;
                cx.background_spawn(
                    async move { sops::decrypt(&environment, &abs_path, format).await },
                )
            })
            .collect();
        cx.spawn(async move |this, cx| {
            for (index, task) in tasks.into_iter().enumerate() {
                let result = task.await;
                this.update(cx, |this, cx| {
                    this.finish_decrypt(index, result);
                    cx.notify();
                })
                .log_err();
            }
        })
    }

    pub fn reload(
        &mut self,
        fs: Arc<dyn Fs>,
        environment: Option<HashMap<String, String>>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let files: Vec<_> = self
            .environments
            .iter()
            .map(|file| (file.abs_path.clone(), file.format))
            .collect();
        cx.spawn(async move |this, cx| {
            let mut states = Vec::new();
            for (abs_path, format) in files {
                states.push(read_state(fs.as_ref(), &abs_path, format).await);
            }
            let decrypt = this
                .update(cx, |this, cx| {
                    for (file, (state, recipients)) in this.environments.iter_mut().zip(states) {
                        file.state = state;
                        file.recipients = recipients;
                    }
                    this.edits.clear();
                    this.added_keys.clear();
                    cx.notify();
                    environment.map(|environment| this.decrypt(environment, cx))
                })
                .log_err()
                .flatten();
            if let Some(decrypt) = decrypt {
                decrypt.await;
            }
        })
    }

    pub fn save(
        &mut self,
        environment: HashMap<String, String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        if self.saving {
            return Task::ready(Err(anyhow::anyhow!("A save is already in progress")));
        }
        let jobs: Vec<_> = self
            .environments
            .iter()
            .zip(self.changes())
            .filter(|(_, changes)| !changes.is_empty())
            .map(|(file, changes)| (file.abs_path.clone(), file.format, changes))
            .collect();
        self.saving = true;
        cx.spawn(async move |this, cx| {
            let saved = cx
                .background_spawn({
                    let environment = environment.clone();
                    async move {
                        for (abs_path, format, changes) in jobs {
                            sops::apply(&environment, &abs_path, format, &changes).await?;
                        }
                        anyhow::Ok(())
                    }
                })
                .await;
            let decrypt = this.update(cx, |this, cx| this.decrypt(environment, cx))?;
            decrypt.await;
            this.update(cx, |this, _| this.saving = false)?;
            saved
        })
    }
}

async fn read_state(
    fs: &dyn Fs,
    abs_path: &Path,
    format: SecretsFormat,
) -> (EnvironmentState, Vec<String>) {
    let text = match fs.load(abs_path).await {
        Ok(text) => text,
        Err(error) => return (locked_with_error(&error), Vec::new()),
    };
    let state = match encrypted_keys(&text, format) {
        Ok(keys) => EnvironmentState::Locked { keys, error: None },
        Err(error) => locked_with_error(&error),
    };
    (state, age_recipients(&text, format))
}

fn locked_with_error(error: &anyhow::Error) -> EnvironmentState {
    EnvironmentState::Locked {
        keys: Vec::new(),
        error: Some(format!("{error:#}").into()),
    }
}

mod open;

#[cfg(test)]
mod tests;
