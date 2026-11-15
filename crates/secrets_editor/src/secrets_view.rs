mod render;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Result;
use collections::HashMap;
use editor::{Editor, EditorEvent, actions::SelectAll};
use gpui::{
    App, AsyncApp, Entity, EventEmitter, FocusHandle, Focusable, ListAlignment, ListState,
    PathPromptOptions, Subscription, Task, WeakEntity,
};
use project::Project;
use ui::{TableInteractionState, prelude::*};
use util::ResultExt as _;
use workspace::{
    Pane, Workspace,
    item::{Item, ItemEvent, ProjectItem, SaveOptions},
};

use crate::{age_key, secret_tree::KeyPath, secrets_group::SecretsGroup};

pub(crate) enum SecretsViewEvent {
    TabChanged,
}

#[derive(Clone)]
enum EditTarget {
    Cell {
        environment_index: usize,
        key: KeyPath,
    },
    NewKey,
}

struct Editing {
    target: EditTarget,
    editor: Entity<Editor>,
    initial_text: String,
    _subscription: Subscription,
}

impl Editing {
    // Compared to the initial text: setting it fires BufferEdited, and the rope normalizes CRLF.
    fn is_edited(&self, cx: &App) -> bool {
        self.editor.read(cx).text(cx) != self.initial_text
    }
}

pub(crate) struct SecretsView {
    group: Entity<SecretsGroup>,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    table_state: Entity<TableInteractionState>,
    list_state: ListState,
    rows: Vec<KeyPath>,
    // Also saved to settings; kept here so they apply before the settings file reloads.
    picked_key_files: Vec<PathBuf>,
    key_sources: Vec<age_key::KeySource>,
    revealed: bool,
    editing: Option<Editing>,
    key_error: Option<SharedString>,
    error: Option<SharedString>,
    unlock_started: bool,
    task: Option<Task<()>>,
    _group_subscription: Subscription,
}

fn display_text(text: &str, revealed: bool) -> String {
    if !revealed {
        return "••••••".to_string();
    }
    match text.split_once('\n') {
        Some((first_line, _)) => format!("{first_line}…"),
        None => text.to_string(),
    }
}

impl SecretsView {
    fn new(project: Entity<Project>, group: Entity<SecretsGroup>, cx: &mut Context<Self>) -> Self {
        let group_subscription = cx.observe(&group, |_, _, cx| {
            cx.emit(SecretsViewEvent::TabChanged);
            cx.notify();
        });
        Self {
            group,
            project,
            workspace: WeakEntity::new_invalid(),
            focus_handle: cx.focus_handle(),
            table_state: cx.new(|cx| TableInteractionState::new(cx)),
            list_state: ListState::new(0, ListAlignment::Top, px(200.)),
            rows: Vec::new(),
            picked_key_files: Vec::new(),
            key_sources: Vec::new(),
            revealed: false,
            editing: None,
            key_error: None,
            error: None,
            unlock_started: false,
            task: None,
            _group_subscription: group_subscription,
        }
    }

    fn update_group(&mut self, cx: &mut Context<Self>, update: impl FnOnce(&mut SecretsGroup)) {
        self.group.update(cx, |group, cx| {
            update(group);
            cx.notify();
        });
    }

    fn show_error(&mut self, error: &anyhow::Error, cx: &mut Context<Self>) {
        log::error!("secrets editor: {error:#}");
        self.error = Some(format!("{error:#}").into());
        cx.notify();
    }

    fn sops_environment(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<HashMap<String, String>>> {
        let directory: Option<Arc<Path>> = self
            .group
            .read(cx)
            .environments()
            .first()
            .and_then(|file| file.abs_path.parent())
            .map(Arc::from);
        let shell_environment = directory.map(|directory| {
            self.project.update(cx, |project, cx| {
                project.environment().update(cx, |environment, cx| {
                    environment.directory_environment(directory, cx)
                })
            })
        });
        let workspace = self.workspace.clone();
        let picked_key_files = self.picked_key_files.clone();
        cx.spawn_in(window, async move |this, cx| {
            let environment = match shell_environment {
                Some(task) => task.await.unwrap_or_default(),
                None => HashMap::default(),
            };
            let keys = age_key::add_keys(workspace, environment, picked_key_files, cx).await?;
            this.update(cx, |this, cx| {
                this.key_sources = keys.sources;
                cx.notify();
            })?;
            Ok(keys.environment)
        })
    }

    fn environment_or_report(
        this: &WeakEntity<Self>,
        result: Result<HashMap<String, String>>,
        cx: &mut AsyncApp,
    ) -> Option<HashMap<String, String>> {
        match result {
            Ok(environment) => Some(environment),
            Err(error) => {
                this.update(cx, |this, cx| this.show_error(&error, cx))
                    .log_err();
                None
            }
        }
    }

    fn unlock(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.error = None;
        let environment = self.sops_environment(window, cx);
        let group = self.group.clone();
        self.task = Some(cx.spawn(async move |this, cx| {
            let Some(environment) = Self::environment_or_report(&this, environment.await, cx)
            else {
                return;
            };
            group
                .update(cx, |group, cx| group.decrypt(environment, cx))
                .await;
        }));
    }

    fn use_age_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Use Age Key".into()),
        });
        let fs = self.project.read(cx).fs().clone();
        cx.spawn_in(window, async move |this, cx| {
            let paths = match paths.await {
                Ok(Ok(Some(paths))) => paths,
                Ok(Err(error)) => {
                    this.update(cx, |this, cx| this.show_error(&error, cx))?;
                    return Ok(());
                }
                Ok(Ok(None)) | Err(_) => return Ok(()),
            };
            this.update_in(cx, |this, window, cx| {
                let stored: Vec<String> = paths
                    .iter()
                    .map(|path| path.to_string_lossy().into_owned())
                    .collect();
                settings::update_settings_file(fs, cx, move |settings, _| {
                    let files = settings
                        .sops
                        .get_or_insert_default()
                        .age_key_files
                        .get_or_insert_default();
                    for path in stored {
                        if !files.contains(&path) {
                            files.push(path);
                        }
                    }
                });
                this.picked_key_files.extend(paths);
                this.unlock(window, cx);
            })
        })
        .detach_and_log_err(cx);
    }

    fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing = None;
        self.error = None;
        let fs = self.project.read(cx).fs().clone();
        let group = self.group.clone();
        let environment = self
            .group
            .read(cx)
            .any_unlocked()
            .then(|| self.sops_environment(window, cx));
        self.task = Some(cx.spawn(async move |this, cx| {
            let environment = match environment {
                Some(task) => Self::environment_or_report(&this, task.await, cx),
                None => None,
            };
            group
                .update(cx, |group, cx| group.reload(fs, environment, cx))
                .await;
        }));
        cx.notify();
    }

    fn open_as_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let path = self.group.read(cx).opened_path().clone();
        let project = self.project.clone();
        let workspace = self.workspace.clone();
        window
            .spawn(cx, async move |cx| {
                let buffer = project
                    .update(cx, |project, cx| project.open_buffer(path, cx))
                    .await?;
                workspace.update_in(cx, |workspace, window, cx| {
                    workspace.open_project_item::<Editor>(
                        None, buffer, true, true, false, true, window, cx,
                    );
                })
            })
            .detach_and_log_err(cx);
    }

    fn start_edit(
        &mut self,
        target: EditTarget,
        initial_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.commit_edit(false, window, cx);
        let editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, 6, window, cx);
            editor.set_text(initial_text, window, cx);
            editor.select_all(&SelectAll, window, cx);
            editor
        });
        let subscription =
            cx.subscribe_in(&editor, window, |this, _, event, window, cx| match event {
                EditorEvent::Blurred => this.commit_edit(true, window, cx),
                EditorEvent::BufferEdited => cx.emit(SecretsViewEvent::TabChanged),
                _ => {}
            });
        window.focus(&editor.focus_handle(cx), cx);
        self.error = None;
        self.key_error = None;
        if matches!(target, EditTarget::NewKey) {
            let index = self.rows.len();
            self.list_state.splice(index..index, 1);
            self.list_state.scroll_to_reveal_item(index);
        }
        self.editing = Some(Editing {
            target,
            editor,
            initial_text: initial_text.replace("\r\n", "\n"),
            _subscription: subscription,
        });
        cx.notify();
    }

    fn commit_edit(&mut self, from_blur: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editing) = self.editing.take() else {
            return;
        };
        let text = editing.editor.read(cx).text(cx);
        match editing.target.clone() {
            // Untouched cells keep their state, so passing over a missing cell does not add "".
            EditTarget::Cell { .. } if !editing.is_edited(cx) => {}
            EditTarget::Cell {
                environment_index,
                key,
            } => self.update_group(cx, |group| group.set_value(environment_index, key, &text)),
            EditTarget::NewKey if text.trim().is_empty() => {}
            EditTarget::NewKey => {
                let added = self.group.update(cx, |group, cx| {
                    cx.notify();
                    group.add_key(&text)
                });
                match added {
                    Ok(key) => {
                        let first_unlocked = self.group.read(cx).first_unlocked();
                        if let Some(environment_index) = first_unlocked {
                            let target = EditTarget::Cell {
                                environment_index,
                                key,
                            };
                            self.start_edit(target, "", window, cx);
                        }
                    }
                    Err(error) if from_blur => self.error = Some(error.to_string().into()),
                    Err(error) => {
                        self.key_error = Some(error.to_string().into());
                        window.focus(&editing.editor.focus_handle(cx), cx);
                        self.editing = Some(editing);
                    }
                }
            }
        }
        if !from_blur && self.editing.is_none() {
            window.focus(&self.focus_handle, cx);
        }
        cx.notify();
    }

    fn confirm_edit(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_edit(false, window, cx);
    }

    fn cancel_edit(
        &mut self,
        _: &editor::actions::Cancel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editing = None;
        self.key_error = None;
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn unset_editing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Editing {
            target:
                EditTarget::Cell {
                    environment_index,
                    key,
                },
            ..
        }) = self.editing.take()
        {
            self.update_group(cx, |group| group.unset(environment_index, key));
        }
        window.focus(&self.focus_handle, cx);
    }
}

impl Focusable for SecretsView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<SecretsViewEvent> for SecretsView {}

impl Item for SecretsView {
    type Event = SecretsViewEvent;

    fn to_item_events(_: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(ItemEvent::UpdateTab);
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        f(self.group.entity_id(), self.group.read(cx));
    }

    fn tab_content_text(&self, _: usize, cx: &App) -> SharedString {
        self.group.read(cx).file_name()
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        Some(self.group.read(cx).title().clone())
    }

    fn tab_icon(&self, _: &Window, _: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Lock))
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.editing
            .as_ref()
            .is_some_and(|editing| editing.is_edited(cx))
            || self.group.read(cx).is_dirty()
    }

    fn can_save(&self, _: &App) -> bool {
        true
    }

    fn save(
        &mut self,
        _: SaveOptions,
        _: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.commit_edit(false, window, cx);
        let environment = self.sops_environment(window, cx);
        let group = self.group.clone();
        cx.spawn(async move |_, cx| {
            let environment = environment.await?;
            group
                .update(cx, |group, cx| group.save(environment, cx))
                .await
        })
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace = workspace.weak_handle();
        if !self.unlock_started {
            self.unlock_started = true;
            self.unlock(window, cx);
        }
    }
}

impl ProjectItem for SecretsView {
    type Item = SecretsGroup;

    fn for_project_item(
        project: Entity<Project>,
        _: Option<&Pane>,
        item: Entity<Self::Item>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new(project, item, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::display_text;

    #[test]
    fn test_display_text() {
        assert_eq!(display_text("secret", false), "••••••");
        assert_eq!(display_text("secret", true), "secret");
        assert_eq!(display_text("first\nsecond", true), "first…");
        assert_eq!(display_text("first\nsecond", false), "••••••");
    }
}
