use editor::Editor;
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, SharedString,
    WeakEntity, Window,
};
use language::language_settings::SoftWrap;
use menu::{Cancel, Confirm};
use theme::ActiveTheme as _;
use ui::{Button, ButtonStyle, Headline, HeadlineSize, prelude::*};
use workspace::{ModalView, Workspace};

type TextCallback = Box<dyn FnOnce(String, &mut Window, &mut App)>;

// Enter inserts a newline in the multi-line editor (full mode), so it confirms through a button.
// An auto-height editor would not do: its plain Enter falls through to `menu::Confirm`.
pub(crate) struct TextModal {
    title: SharedString,
    icon: IconName,
    confirm_label: Option<SharedString>,
    editor: Entity<Editor>,
    on_confirm: Option<TextCallback>,
}

impl TextModal {
    pub(crate) fn open_single_line(
        workspace: &WeakEntity<Workspace>,
        title: impl Into<SharedString>,
        icon: IconName,
        initial_text: &str,
        window: &mut Window,
        cx: &mut App,
        on_confirm: impl FnOnce(String, &mut Window, &mut App) + 'static,
    ) {
        let title = title.into();
        workspace
            .update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    let editor = cx.new(|cx| Editor::single_line(window, cx));
                    Self::new(
                        title,
                        icon,
                        None,
                        editor,
                        initial_text,
                        Box::new(on_confirm),
                        window,
                        cx,
                    )
                });
            })
            .ok();
    }

    pub(crate) fn multi_line(
        title: SharedString,
        confirm_label: &'static str,
        initial_text: &str,
        on_confirm: TextCallback,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_show_gutter(false, cx);
            editor.set_show_wrap_guides(false, cx);
            editor.set_show_indent_guides(false, cx);
            editor.set_soft_wrap_mode(SoftWrap::EditorWidth, cx);
            editor
        });
        Self::new(
            title,
            IconName::Pencil,
            Some(confirm_label.into()),
            editor,
            initial_text,
            on_confirm,
            window,
            cx,
        )
    }

    fn new(
        title: SharedString,
        icon: IconName,
        confirm_label: Option<SharedString>,
        editor: Entity<Editor>,
        initial_text: &str,
        on_confirm: TextCallback,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        editor.update(cx, |editor, cx| editor.set_text(initial_text, window, cx));
        Self {
            title,
            icon,
            confirm_label,
            editor,
            on_confirm: Some(on_confirm),
        }
    }

    fn cancel(&mut self, _: &Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).text(cx).trim().to_string();
        if !text.is_empty()
            && let Some(on_confirm) = self.on_confirm.take()
        {
            on_confirm(text, window, cx);
        }
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for TextModal {}
impl ModalView for TextModal {}
impl Focusable for TextModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for TextModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let border_color = cx.theme().colors().border_variant;
        v_flex()
            .key_context("TextModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_2(cx)
            .w(rems(if self.confirm_label.is_some() {
                40.
            } else {
                34.
            }))
            .child(
                h_flex()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .w_full()
                    .gap_1p5()
                    .child(Icon::new(self.icon).size(IconSize::XSmall))
                    .child(Headline::new(self.title.clone()).size(HeadlineSize::XSmall)),
            )
            .map(|modal| match self.confirm_label.clone() {
                None => modal.child(div().px_3().pb_3().w_full().child(self.editor.clone())),
                Some(confirm_label) => modal
                    .child(
                        div()
                            .mx_3()
                            .p_1()
                            .h(rems(12.))
                            .overflow_hidden()
                            .border_1()
                            .border_color(border_color)
                            .rounded_sm()
                            .child(self.editor.clone()),
                    )
                    .child(
                        h_flex()
                            .p_2()
                            .gap_2()
                            .justify_end()
                            .child(
                                Button::new("text-modal-cancel", "Cancel")
                                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                            )
                            .child(
                                Button::new("text-modal-confirm", confirm_label)
                                    .style(ButtonStyle::Filled)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.confirm(&Confirm, window, cx)
                                    })),
                            ),
                    ),
            })
    }
}
