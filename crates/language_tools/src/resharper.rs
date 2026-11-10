//! Fork: the UI side of the ReSharper server - account actions and what its commands ask of the editor.
use std::{future::Future, sync::Arc};

use anyhow::{Context as _, Result};
use collections::HashSet;
use editor::{Editor, MultiBufferOffset};
use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{App, Context, DismissEvent, Entity, Task, Window, actions};
use language::{LineEnding, OffsetRangeExt as _, range_from_lsp};
use picker::{Picker, PickerDelegate};
use project::lsp_store::resharper_ext::{self, Reply, UiRequest};
use snippet::Snippet;
use ui::{HighlightedLabel, ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;
use workspace::{Workspace, notifications::DetachAndPromptErr as _};

actions!(
    resharper,
    [
        /// Signs in to a JetBrains Account to license ReSharper.
        SignIn,
        /// Signs out of the JetBrains Account that licenses ReSharper.
        SignOut,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        workspace.register_action(|workspace, _: &SignIn, window, cx| {
            let sign_in = resharper_ext::sign_in(&workspace.project().read(cx).lsp_store(), cx);
            prompt_on_error(sign_in, "ReSharper sign-in failed", window, cx);
        });
        workspace.register_action(|workspace, _: &SignOut, window, cx| {
            let sign_out = resharper_ext::sign_out(&workspace.project().read(cx).lsp_store(), cx);
            prompt_on_error(sign_out, "ReSharper sign-out failed", window, cx);
        });
        let Some(window) = window else {
            return;
        };
        let lsp_store = workspace.project().read(cx).lsp_store();
        cx.subscribe_in(&lsp_store, window, handle_ui_request)
            .detach();
    })
    .detach();
}

fn prompt_on_error(
    task: impl Future<Output = Result<()>> + 'static,
    message: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    cx.spawn(async move |_, _| task.await)
        .detach_and_prompt_err(message, window, cx, |_, _, _| None);
}

fn handle_ui_request(
    workspace: &mut Workspace,
    _: &Entity<project::LspStore>,
    request: &UiRequest,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    match request {
        UiRequest::ApplySnippet {
            edits,
            snippet,
            snippet_range,
            applied,
        } => {
            let result = apply_snippet(workspace, edits, snippet, *snippet_range, window, cx);
            if result.log_err().is_some() {
                applied.try_send(()).log_err();
            }
        }
        UiRequest::Pick {
            title,
            labels,
            many,
            picked,
        } => {
            let delegate = QuickPick {
                placeholder: placeholder(title.as_deref(), *many),
                candidates: labels
                    .iter()
                    .enumerate()
                    .map(|(id, label)| StringMatchCandidate::new(id, label))
                    .collect(),
                matches: Vec::new(),
                selected_index: 0,
                checked: many.then(HashSet::default),
                picked: picked.clone(),
            };
            workspace.toggle_modal(window, cx, |window, cx| {
                Picker::uniform_list(delegate, window, cx)
            });
        }
    }
}

fn placeholder(title: Option<&str>, many: bool) -> Arc<str> {
    let title = title.unwrap_or("ReSharper");
    if many {
        format!("{title} (tab picks several)").into()
    } else {
        title.into()
    }
}

/// The backend means the active editor: its request names no document.
fn apply_snippet(
    workspace: &Workspace,
    edits: &[lsp::TextEdit],
    snippet: &str,
    snippet_range: lsp::Range,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Result<()> {
    let editor = workspace
        .active_item_as::<Editor>(cx)
        .context("the active item is not an editor")?;
    // Buffers hold `\n` only, so a `\r` would shift every tab stop after it.
    let snippet = Snippet::parse(&LineEnding::normalize_cow(snippet.into()))?;
    editor.update(cx, |editor, cx| {
        let buffer = editor
            .active_buffer(cx)
            .context("the editor has no buffer")?;
        let is_singleton = editor.buffer().read(cx).is_singleton();
        editor.transact(window, cx, |editor, window, cx| {
            buffer.update(cx, |buffer, cx| {
                let snapshot = buffer.text_snapshot();
                let mut edits: Vec<_> = edits
                    .iter()
                    .map(|edit| {
                        let range = range_from_lsp(edit.range).to_offset(&snapshot);
                        (range, edit.new_text.clone())
                    })
                    .collect();
                edits.sort_by_key(|(range, _)| range.start);
                buffer.edit(edits, None, cx);
            });
            // In a multibuffer the edits already left the snippet's text, only its tab stops are lost.
            if is_singleton {
                let snapshot = buffer.read(cx).text_snapshot();
                let range = range_from_lsp(snippet_range).to_offset(&snapshot);
                let range = MultiBufferOffset(range.start)..MultiBufferOffset(range.end);
                // The backend already indented the text.
                editor
                    .insert_snippet_with_autoindent(&[range], snippet, None, window, cx)
                    .log_err();
            }
        });
        Ok(())
    })
}

struct QuickPick {
    placeholder: Arc<str>,
    candidates: Vec<StringMatchCandidate>,
    matches: Vec<StringMatch>,
    selected_index: usize,
    /// Candidate ids; `Some` when several items can be picked.
    checked: Option<HashSet<usize>>,
    picked: Reply<Option<Vec<usize>>>,
}

impl QuickPick {
    fn candidate_id(&self, index: usize) -> Option<usize> {
        Some(self.matches.get(index)?.candidate_id)
    }

    fn pick(&mut self, picked: Vec<usize>, cx: &mut Context<Picker<Self>>) {
        self.picked.try_send(Some(picked)).log_err();
        cx.emit(DismissEvent);
    }
}

impl PickerDelegate for QuickPick {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "resharper quick pick"
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        self.placeholder.clone()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, index: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected_index = index;
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let candidates = self.candidates.clone();
        let executor = cx.background_executor().clone();
        cx.spawn_in(window, async move |picker, cx| {
            let cancelled = Default::default();
            let matches =
                match_strings(&candidates, &query, false, true, 100, &cancelled, executor).await;
            picker
                .update(cx, |picker, cx| {
                    picker.delegate.matches = matches;
                    picker.delegate.selected_index = 0;
                    cx.notify();
                })
                .log_err();
        })
    }

    fn confirm(&mut self, _: bool, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(selected) = self.candidate_id(self.selected_index) {
            self.pick(vec![selected], cx);
        }
    }

    fn supports_multi_select(&self) -> bool {
        self.checked.is_some()
    }

    fn is_item_selected(&self, index: usize) -> bool {
        let checked = self.checked.as_ref();
        checked
            .zip(self.candidate_id(index))
            .is_some_and(|(checked, id)| checked.contains(&id))
    }

    fn toggle_item_selected(
        &mut self,
        index: usize,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) {
        let id = self.candidate_id(index);
        if let Some((checked, id)) = self.checked.as_mut().zip(id)
            && !checked.remove(&id)
        {
            checked.insert(id);
        }
    }

    fn selected_item_count(&self) -> usize {
        self.checked.as_ref().map_or(0, HashSet::len)
    }

    fn clear_selection(&mut self, _: &mut Context<Picker<Self>>) {
        if let Some(checked) = &mut self.checked {
            checked.clear();
        }
    }

    fn confirm_multi(&mut self, _: bool, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        let mut picked: Vec<usize> = self.checked.iter().flatten().copied().collect();
        picked.sort_unstable();
        self.pick(picked, cx);
    }

    fn dismissed(&mut self, _: &mut Window, _: &mut Context<Picker<Self>>) {
        // Full once an answer was picked, which also dismisses.
        self.picked.try_send(None).ok();
    }

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let item = self.matches.get(index)?;
        Some(
            ListItem::new(index)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(HighlightedLabel::new(
                    item.string.clone(),
                    item.positions.clone(),
                )),
        )
    }
}

#[cfg(test)]
mod tests;
