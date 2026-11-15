use gpui::AnyElement;
use ui::{Banner, ColumnWidthConfig, Table, Tooltip, prelude::*};

use super::{EditTarget, Editing, SecretsView, display_text};
use crate::{
    secret_tree::KeyPath,
    secrets_group::{Cell, EnvironmentFile, EnvironmentState},
};

impl SecretsView {
    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let group = self.group.read(cx);
        let any_locked = group.any_locked();
        let any_unlocked = group.any_unlocked();
        let eye = if self.revealed {
            IconName::EyeOff
        } else {
            IconName::Eye
        };
        let button = |id: &'static str, label: &'static str, icon: IconName| {
            Button::new(id, label)
                .style(ButtonStyle::Subtle)
                .start_icon(Icon::new(icon).size(IconSize::Small))
        };
        h_flex()
            .w_full()
            .p_2()
            .gap_2()
            .justify_between()
            .child(Label::new(group.title().clone()))
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        button("reveal", if self.revealed { "Hide" } else { "Reveal" }, eye)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.revealed = !this.revealed;
                                cx.notify();
                            })),
                    )
                    .child(
                        button("add-key", "Add Key", IconName::Plus)
                            .disabled(!any_unlocked)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.start_edit(EditTarget::NewKey, "", window, cx)
                            })),
                    )
                    .child(
                        button("reload", "Reload", IconName::RotateCw)
                            .on_click(cx.listener(|this, _, window, cx| this.reload(window, cx))),
                    )
                    .when(any_locked, |this| {
                        this.child(
                            button("unlock", "Unlock", IconName::LockOff).on_click(
                                cx.listener(|this, _, window, cx| this.unlock(window, cx)),
                            ),
                        )
                    })
                    .child(
                        button("open-as-text", "Open as Text", IconName::FileCode).on_click(
                            cx.listener(|this, _, window, cx| this.open_as_text(window, cx)),
                        ),
                    ),
            )
    }

    fn render_banners(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let group = self.group.read(cx);
        // (severity, message, whether choosing another age key can help)
        let mut banners = Vec::new();
        let missing = group.missing();
        if !missing.is_empty() {
            let details = missing
                .iter()
                .map(|(name, keys)| {
                    let keys: Vec<String> = keys.iter().map(ToString::to_string).collect();
                    format!("{name}: {}", keys.join(", "))
                })
                .collect::<Vec<_>>()
                .join("; ");
            banners.push((Severity::Warning, format!("Missing keys. {details}"), false));
        }
        for file in group.environments() {
            if let EnvironmentState::Locked {
                error: Some(error), ..
            } = &file.state
            {
                let mut message = format!("{}: {error}", file.name);
                if !file.recipients.is_empty() {
                    message.push_str(&format!(
                        ". It is encrypted for {}",
                        file.recipients.join(", ")
                    ));
                }
                banners.push((Severity::Error, message, !file.recipients.is_empty()));
            }
        }
        if let Some(error) = &self.error {
            banners.push((Severity::Error, error.to_string(), false));
        }
        v_flex().px_2().gap_1().children(banners.into_iter().enumerate().map(
            |(index, (severity, message, offer_key))| {
                Banner::new()
                    .severity(severity)
                    .wrap_content(true)
                    .child(Label::new(message))
                    .when(offer_key, |banner| {
                        banner.action_slot(
                            Button::new(("use-age-key", index), "Use Age Key")
                                .tooltip(Tooltip::text(
                                    "Choose age key files to decrypt with. They are saved to settings.",
                                ))
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.use_age_key(window, cx)),
                                ),
                        )
                    })
            },
        ))
    }

    fn render_environment_header(&self, index: usize, file: &EnvironmentFile) -> AnyElement {
        let (icon, color, title, detail): (_, _, SharedString, Option<SharedString>) =
            match &file.state {
                EnvironmentState::Unlocked { .. } => {
                    match self
                        .key_sources
                        .iter()
                        .find(|source| file.recipients.contains(&source.recipient))
                    {
                        Some(source) => (
                            IconName::LockOff,
                            Color::Success,
                            format!("Decrypted with {}", source.origin).into(),
                            Some(source.recipient.clone().into()),
                        ),
                        None => (
                            IconName::LockOff,
                            Color::Success,
                            "Decrypted by sops".into(),
                            None,
                        ),
                    }
                }
                EnvironmentState::Locked { error, .. } => (
                    IconName::Lock,
                    if error.is_some() {
                        Color::Error
                    } else {
                        Color::Muted
                    },
                    "Locked".into(),
                    (!file.recipients.is_empty())
                        .then(|| format!("Encrypted for {}", file.recipients.join(", ")).into()),
                ),
            };
        h_flex()
            .id(("environment-header", index))
            .gap_1()
            .child(Icon::new(icon).size(IconSize::XSmall).color(color))
            .child(Label::new(file.name.clone()))
            .tooltip(move |_, cx| match &detail {
                Some(detail) => Tooltip::with_meta(title.clone(), None, detail.clone(), cx),
                None => Tooltip::simple(title.clone(), cx),
            })
            .into_any_element()
    }

    fn render_edit_field(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(editing) = &self.editing else {
            return div().into_any_element();
        };
        let is_cell = matches!(editing.target, EditTarget::Cell { .. });
        let removable = matches!(&editing.target, EditTarget::Cell { key, .. } if !key.has_index());
        h_flex()
            .w_full()
            .gap_1()
            .on_action(cx.listener(Self::confirm_edit))
            .on_action(cx.listener(Self::cancel_edit))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .px_1()
                    .border_1()
                    .border_color(cx.theme().colors().border_focused)
                    .rounded_sm()
                    .child(editing.editor.clone()),
            )
            .when(removable, |this| {
                this.child(
                    IconButton::new("unset-value", IconName::Trash)
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Muted)
                        .tooltip(Tooltip::text("Remove value from this environment"))
                        .on_click(
                            cx.listener(|this, _, window, cx| this.unset_editing(window, cx)),
                        ),
                )
            })
            .when_some(
                self.key_error.clone().filter(|_| !is_cell),
                |this, error| {
                    this.child(Label::new(error).color(Color::Error).size(LabelSize::Small))
                },
            )
            .into_any_element()
    }

    fn render_key_cell(
        &mut self,
        row_index: usize,
        key: &KeyPath,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = key.clone();
        h_flex()
            .w_full()
            .justify_between()
            .child(Label::new(key.to_string()).single_line())
            .when(!key.has_index(), |this| {
                this.child(
                    IconButton::new(("remove-key", row_index), IconName::Trash)
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Muted)
                        .tooltip(Tooltip::text("Remove key from all environments"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.editing = None;
                            this.update_group(cx, |group| group.remove_key(&key));
                        })),
                )
            })
            .into_any_element()
    }

    fn render_value_cell(
        &mut self,
        environment_index: usize,
        key: &KeyPath,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if let Some(Editing {
            target:
                EditTarget::Cell {
                    environment_index: editing_index,
                    key: editing_key,
                },
            ..
        }) = &self.editing
            && *editing_index == environment_index
            && editing_key == key
        {
            return self.render_edit_field(cx);
        }

        let group = self.group.read(cx);
        let unlocked = group
            .environments()
            .get(environment_index)
            .is_some_and(|file| file.is_unlocked());
        let (label, initial_text) = match group.cell(environment_index, key) {
            Cell::Locked => {
                return Label::new("locked").color(Color::Muted).into_any_element();
            }
            Cell::Missing if !unlocked => {
                return Label::new("missing")
                    .color(Color::Warning)
                    .into_any_element();
            }
            Cell::Missing => (Label::new("missing").color(Color::Warning), String::new()),
            Cell::Removed => (Label::new("removed").color(Color::Deleted), String::new()),
            Cell::Value { text, edited } => {
                let color = if edited {
                    Color::Modified
                } else {
                    Color::Default
                };
                (
                    Label::new(display_text(&text, self.revealed))
                        .single_line()
                        .color(color),
                    text,
                )
            }
        };
        let key = key.clone();
        div()
            .id(("secret-cell", environment_index))
            .w_full()
            .cursor_pointer()
            .child(label)
            .on_click(cx.listener(move |this, _, window, cx| {
                let target = EditTarget::Cell {
                    environment_index,
                    key: key.clone(),
                };
                this.start_edit(target, &initial_text, window, cx);
            }))
            .into_any_element()
    }

    fn render_row(
        &mut self,
        row_index: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let environment_count = self.group.read(cx).environments().len();
        let mut cells = Vec::with_capacity(environment_count + 1);
        match self.rows.get(row_index).cloned() {
            Some(key) => {
                cells.push(self.render_key_cell(row_index, &key, cx));
                for environment_index in 0..environment_count {
                    cells.push(self.render_value_cell(environment_index, &key, cx));
                }
            }
            None => {
                cells.push(self.render_edit_field(cx));
                cells.extend((0..environment_count).map(|_| div().into_any_element()));
            }
        }
        cells
    }
}

impl Render for SecretsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.rows = self.group.read(cx).rows();
        let (environment_headers, row_count) = {
            let group = self.group.read(cx);
            let headers: Vec<AnyElement> = group
                .environments()
                .iter()
                .enumerate()
                .map(|(index, file)| self.render_environment_header(index, file))
                .collect();
            let new_key_row = matches!(
                self.editing,
                Some(Editing {
                    target: EditTarget::NewKey,
                    ..
                })
            );
            (headers, self.rows.len() + usize::from(new_key_row))
        };
        // Visible rows are remeasured every frame; only the count needs syncing. Splice keeps the scroll position.
        let previous_count = self.list_state.item_count();
        if previous_count != row_count {
            self.list_state.splice(
                row_count.min(previous_count)..previous_count,
                row_count.saturating_sub(previous_count),
            );
        }
        let mut header = vec![Label::new("Key").into_any_element()];
        header.extend(environment_headers);
        let toolbar = self.render_toolbar(cx).into_any_element();
        let banners = self.render_banners(cx).into_any_element();
        v_flex()
            .size_full()
            .key_context("SecretsView")
            .track_focus(&self.focus_handle)
            .child(toolbar)
            .child(banners)
            .child(
                Table::new(header.len())
                    .interactable(&self.table_state)
                    .striped()
                    .width_config(ColumnWidthConfig::auto())
                    .header(header)
                    .variable_row_height_list(
                        row_count,
                        self.list_state.clone(),
                        cx.processor(Self::render_row),
                    ),
            )
    }
}
