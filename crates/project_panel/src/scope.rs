// Fork: Visual Studio-like "Scope to This", the panel shows a single folder as its root.
use std::sync::Arc;

use gpui::{App, Context, Window, actions};
use project::{Entry, ProjectEntryId, WorktreeId};
use ui::{ListItem, Tooltip, prelude::*};
use util::rel_path::RelPath;

use crate::ProjectPanel;

actions!(
    project_panel,
    [
        /// Shows only the selected folder in the project panel.
        ScopeToFolder,
        /// Scopes the project panel to the parent of the current scope.
        ScopeToParent,
        /// Shows the whole project in the project panel again.
        ResetScope,
    ]
);

impl ProjectPanel {
    /// Drops the scope once its folder or worktree is gone.
    pub(crate) fn scope_entry(&mut self, cx: &App) -> Option<(WorktreeId, Entry)> {
        let (worktree_id, entry_id) = self.scope?;
        let entry = self
            .project
            .read(cx)
            .worktree_for_id(worktree_id, cx)
            .and_then(|worktree| worktree.read(cx).entry_for_id(entry_id).cloned());
        if entry.is_none() {
            self.scope = None;
        }
        Some((worktree_id, entry?))
    }

    pub(crate) fn scope_path(&self, cx: &App) -> Option<Arc<RelPath>> {
        let (worktree_id, entry_id) = self.scope?;
        let worktree = self.project.read(cx).worktree_for_id(worktree_id, cx)?;
        Some(worktree.read(cx).entry_for_id(entry_id)?.path.clone())
    }

    pub(crate) fn scope_contains(&self, entry_id: ProjectEntryId, cx: &App) -> bool {
        let Some((worktree_id, scope_id)) = self.scope else {
            return true;
        };
        self.project
            .read(cx)
            .worktree_for_id(worktree_id, cx)
            .is_some_and(|worktree| {
                let worktree = worktree.read(cx);
                worktree
                    .entry_for_id(entry_id)
                    .zip(worktree.entry_for_id(scope_id))
                    .is_some_and(|(entry, scope)| entry.path.starts_with(&scope.path))
            })
    }

    pub(crate) fn scope_to_folder(
        &mut self,
        _: &ScopeToFolder,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some((worktree, entry)) = self.selected_sub_entry(cx)
            && entry.is_dir()
        {
            let scope = (worktree.read(cx).id(), entry.id);
            self.set_scope(Some(scope), Some(scope), window, cx);
        }
    }

    pub(crate) fn scope_to_parent(
        &mut self,
        _: &ScopeToParent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((worktree_id, entry)) = self.scope_entry(cx) else {
            return;
        };
        let parent = self
            .project
            .read(cx)
            .worktree_for_id(worktree_id, cx)
            .and_then(|worktree| worktree.read(cx).entry_for_path(entry.path.parent()?))
            .filter(|parent| !parent.path.is_empty())
            .map(|parent| (worktree_id, parent.id));
        self.set_scope(parent, Some((worktree_id, entry.id)), window, cx);
    }

    pub(crate) fn reset_scope(
        &mut self,
        _: &ResetScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let selection = self
            .selection
            .map(|selection| (selection.worktree_id, selection.entry_id));
        self.set_scope(None, selection, window, cx);
    }

    fn set_scope(
        &mut self,
        scope: Option<(WorktreeId, ProjectEntryId)>,
        selection: Option<(WorktreeId, ProjectEntryId)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some((worktree_id, entry_id)) = scope {
            self.expand_entry(worktree_id, entry_id, cx);
        }
        self.scope = scope;
        self.update_visible_entries(selection, false, true, window, cx);
        cx.notify();
    }

    pub(crate) fn render_scope_header(&self, cx: &Context<Self>) -> Option<impl IntoElement> {
        self.scope?;
        Some(
            div()
                .border_b_1()
                .border_color(cx.theme().colors().border_variant)
                .child(
                    ListItem::new("project-panel-scope-parent")
                        .start_slot(
                            Icon::new(IconName::ArrowUp)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .child(Label::new("..").color(Color::Muted))
                        .end_slot(
                            IconButton::new("project-panel-reset-scope", IconName::Close)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::for_action_title("Reset Scope", &ResetScope))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.reset_scope(&ResetScope, window, cx)
                                })),
                        )
                        .tooltip(Tooltip::for_action_title("Scope to Parent", &ScopeToParent))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.scope_to_parent(&ScopeToParent, window, cx)
                        })),
                ),
        )
    }
}
