use std::path::Path;

use editor::Editor;
use futures::channel::oneshot;
use gpui::{DismissEvent, Entity, EventEmitter, FocusHandle, Focusable};
use ui::{Checkbox, Headline, HeadlineSize, ToggleState, prelude::*};
use workspace::{ModalView, Workspace};
use zeroize::Zeroizing;

pub(crate) struct PassphraseResponse {
    pub passphrase: Zeroizing<String>,
    pub remember: bool,
}

pub(crate) struct PassphraseModal {
    key_file: SharedString,
    error: Option<SharedString>,
    editor: Entity<Editor>,
    remember: bool,
    sender: Option<oneshot::Sender<Option<PassphraseResponse>>>,
}

impl EventEmitter<DismissEvent> for PassphraseModal {}
impl ModalView for PassphraseModal {}

impl Focusable for PassphraseModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Drop for PassphraseModal {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            sender.send(None).ok();
        }
    }
}

pub(crate) fn prompt(
    workspace: &mut Workspace,
    key_file: &Path,
    error: Option<SharedString>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> oneshot::Receiver<Option<PassphraseResponse>> {
    let (sender, receiver) = oneshot::channel();
    let key_file = SharedString::from(key_file.display().to_string());
    workspace.toggle_modal(window, cx, |window, cx| {
        PassphraseModal::new(key_file, error, sender, window, cx)
    });
    receiver
}

impl PassphraseModal {
    fn new(
        key_file: SharedString,
        error: Option<SharedString>,
        sender: oneshot::Sender<Option<PassphraseResponse>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_masked(true, cx);
            editor.set_placeholder_text("Passphrase", window, cx);
            editor
        });
        Self {
            key_file,
            error,
            editor,
            remember: false,
            sender: Some(sender),
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(sender) = self.sender.take() {
            let passphrase = self.editor.update(cx, |editor, cx| {
                let passphrase = Zeroizing::new(editor.text(cx));
                editor.clear(window, cx);
                passphrase
            });
            sender
                .send(Some(PassphraseResponse {
                    passphrase,
                    remember: self.remember,
                }))
                .ok();
        }
        cx.emit(DismissEvent);
    }
}

impl Render for PassphraseModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("PassphrasePrompt")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_2(cx)
            .w_96()
            .child(
                v_flex()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .gap_0p5()
                    .child(Headline::new("Unlock age key").size(HeadlineSize::XSmall))
                    .child(
                        Label::new(self.key_file.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(
                v_flex()
                    .py_2()
                    .px_3()
                    .gap_2()
                    .bg(cx.theme().colors().editor_background)
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(self.editor.clone())
                    .child(
                        Checkbox::new("remember-in-keychain", ToggleState::from(self.remember))
                            .label("Remember in Keychain")
                            .on_click(cx.listener(|this, state: &ToggleState, _, cx| {
                                this.remember = state.selected();
                                cx.notify();
                            })),
                    )
                    .children(
                        self.error.clone().map(|error| {
                            Label::new(error).size(LabelSize::Small).color(Color::Error)
                        }),
                    ),
            )
    }
}
