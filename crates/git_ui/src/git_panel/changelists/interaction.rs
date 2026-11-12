use super::ChangelistId;
use crate::{
    git_panel::{GitHeaderEntry, GitPanel, GitStatusEntry, GitTreeDirEntry, Section, TreeKey},
    git_text_modal::TextModal,
};
use git::repository::RepoPath;
use gpui::{
    Action as _, App, Context, MouseButton, MouseDownEvent, Pixels, Point, Render, SharedString,
    StatefulInteractiveElement, StyleRefinement, WeakEntity, Window, actions,
};
use project::git_store::Repository;
use theme::ActiveTheme as _;
use ui::{ContextMenu, IconPosition, prelude::*};
use workspace::DraggedSelection;

actions!(
    git_panel,
    [
        /// Creates an empty changelist.
        NewChangelist
    ]
);

/// What a drag from the changes list carries. Rows are rebuilt every frame, so the paths are
/// collected only when the drag starts or lands.
pub(in crate::git_panel) enum DraggedChanges {
    /// The dragged row is part of the marks, so all of them move.
    Marked,
    File(RepoPath),
    Directory(TreeKey),
}

struct DraggedChangesPreview(SharedString);

impl Render for DraggedChangesPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        h_flex()
            .px_2()
            .py_0p5()
            .rounded_sm()
            .border_1()
            .border_color(colors.border)
            .bg(colors.elevated_surface_background)
            .child(Label::new(self.0.clone()).size(LabelSize::Small))
    }
}

fn drop_target_style<T>(
    style: StyleRefinement,
    _: &T,
    _: &mut Window,
    cx: &mut App,
) -> StyleRefinement {
    style.bg(cx.theme().colors().drop_target_background)
}

type PanelHandle = WeakEntity<GitPanel>;

fn panel_handler(
    panel: &PanelHandle,
    run: impl Fn(&mut GitPanel, &mut Window, &mut Context<GitPanel>) + 'static,
) -> impl Fn(&mut Window, &mut App) + 'static {
    let panel = panel.clone();
    move |window, cx| {
        panel.update(cx, |this, cx| run(this, window, cx)).ok();
    }
}

impl GitPanel {
    pub(in crate::git_panel) fn header_title(&self, header: &GitHeaderEntry) -> SharedString {
        match header.header {
            Section::Changelist(id) => self
                .changelists
                .model()
                .name(id)
                .cloned()
                .unwrap_or_else(|| header.title().into()),
            _ => header.title().into(),
        }
    }

    /// Like [`GitHeaderEntry::contains`], but aware of which files sit in a changelist.
    pub(in crate::git_panel) fn header_contains(
        &self,
        header: &GitHeaderEntry,
        entry: &GitStatusEntry,
        repo: &Repository,
    ) -> bool {
        match header.header {
            Section::Changelist(id) => self.changelists.is_in(&entry.repo_path, id),
            Section::Tracked | Section::New if self.changelists.is_in_list(&entry.repo_path) => {
                false
            }
            _ => header.contains(entry, repo),
        }
    }

    /// Makes a list header accept drops and, for changelists, open their own menu.
    pub(in crate::git_panel) fn changelist_header<E: StatefulInteractiveElement>(
        &self,
        header: E,
        section: Section,
        cx: &Context<Self>,
    ) -> E {
        if !self.changelists.is_active() {
            return header;
        }
        let list = match section {
            Section::Changelist(id) => Some(id),
            Section::Tracked | Section::New => None,
            _ => return header,
        };
        let header = header
            .drag_over::<DraggedChanges>(drop_target_style)
            .drag_over::<DraggedSelection>(drop_target_style)
            .on_drop(
                cx.listener(move |this, dragged: &DraggedChanges, window, cx| {
                    this.assign_changes(this.dragged_paths(dragged), list, window, cx);
                }),
            )
            .on_drop(
                cx.listener(move |this, selection: &DraggedSelection, window, cx| {
                    let paths = this.selection_paths(selection, cx);
                    this.assign_changes(paths, list, window, cx);
                }),
            );
        match section {
            Section::Changelist(id) => header.on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.deploy_changelist_menu(event.position, id, window, cx);
                    cx.stop_propagation();
                }),
            ),
            _ => header,
        }
    }

    /// The row's file, or everything marked when the row is part of the marks.
    pub(in crate::git_panel) fn dragged_file(
        &self,
        ix: usize,
        entry: &GitStatusEntry,
    ) -> DraggedChanges {
        if self.row_is_marked(ix) {
            DraggedChanges::Marked
        } else {
            DraggedChanges::File(entry.repo_path.clone())
        }
    }

    /// Everything under the folder, or everything marked when the folder is part of the marks.
    pub(in crate::git_panel) fn dragged_directory(
        &self,
        directory: &GitTreeDirEntry,
    ) -> DraggedChanges {
        if self.marked_directories.contains(&directory.key) {
            DraggedChanges::Marked
        } else {
            DraggedChanges::Directory(directory.key.clone())
        }
    }

    /// Makes a row draggable onto a list header.
    pub(in crate::git_panel) fn changelist_drag_source<E: StatefulInteractiveElement>(
        &self,
        row: E,
        dragged: DraggedChanges,
        cx: &Context<Self>,
    ) -> E {
        if !self.changelists.is_active() {
            return row;
        }
        let panel = cx.weak_entity();
        row.on_drag(dragged, move |dragged, _, _, cx| {
            let label = panel
                .read_with(cx, |panel, _| panel.drag_label(dragged))
                .unwrap_or_default();
            cx.new(|_| DraggedChangesPreview(label))
        })
    }

    fn drag_label(&self, dragged: &DraggedChanges) -> SharedString {
        match self.dragged_paths(dragged).as_slice() {
            [only] => only
                .file_name()
                .map_or_else(|| only.as_unix_str().to_string(), str::to_string)
                .into(),
            many => format!("{} files", many.len()).into(),
        }
    }

    /// Dragging marked rows moves all marks, deleted files included: the selection leaves
    /// those out because they have no project entry.
    pub(in crate::git_panel) fn selection_paths(
        &self,
        selection: &DraggedSelection,
        cx: &App,
    ) -> Vec<RepoPath> {
        let Some(repository) = self.active_repository.as_ref() else {
            return Vec::new();
        };
        let (project, repository) = (self.project.read(cx), repository.read(cx));
        let paths: Vec<RepoPath> = selection
            .items()
            .filter_map(|item| {
                let project_path = project.path_for_entry(item.entry_id, cx)?;
                repository.project_path_to_repo_path(&project_path, cx)
            })
            .collect();
        let is_marked = |path: &RepoPath| {
            self.marked_entries
                .iter()
                .chain(&self.marked_directories)
                .any(|key| key.path == *path)
        };
        if paths.len() > 1 && paths.iter().all(is_marked) {
            self.dragged_paths(&DraggedChanges::Marked)
        } else {
            paths
        }
    }

    pub(in crate::git_panel) fn dragged_paths(&self, dragged: &DraggedChanges) -> Vec<RepoPath> {
        match dragged {
            // Not `effective_status_entries`: that prefers the keyboard selection over a lone mark.
            DraggedChanges::Marked => self
                .marked_file_entries()
                .into_iter()
                .map(|entry| entry.repo_path)
                .collect(),
            DraggedChanges::File(path) => vec![path.clone()],
            DraggedChanges::Directory(key) => self
                .view_mode
                .tree_state()
                .and_then(|state| state.directory_descendants.get(key))
                .into_iter()
                .flatten()
                .map(|entry| entry.repo_path.clone())
                .collect(),
        }
    }

    pub(in crate::git_panel) fn assign_changes(
        &mut self,
        paths: Vec<RepoPath>,
        list: Option<ChangelistId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.changelists
            .update(cx, |model| model.assign(paths, list));
        self.update_visible_entries(window, cx);
        cx.notify();
    }

    /// `paths` move into the new list; pass none for an empty one.
    pub(in crate::git_panel) fn prompt_new_changelist(
        &mut self,
        paths: Vec<RepoPath>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let panel = cx.weak_entity();
        TextModal::open_single_line(
            &self.workspace,
            "New Changelist",
            IconName::ListTree,
            "",
            window,
            cx,
            move |name, window, cx| {
                panel
                    .update(cx, |this, cx| {
                        this.changelists.update(cx, |model| {
                            if let Some(id) = model.create(&name) {
                                model.assign(paths, Some(id));
                            }
                        });
                        this.update_visible_entries(window, cx);
                        cx.notify();
                    })
                    .ok();
            },
        );
    }

    pub(in crate::git_panel) fn new_changelist(
        &mut self,
        _: &NewChangelist,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.changelists.is_active() {
            self.prompt_new_changelist(Vec::new(), window, cx);
        }
    }

    fn prompt_rename_changelist(
        &mut self,
        id: ChangelistId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(current) = self.changelists.model().name(id).cloned() else {
            return;
        };
        let panel = cx.weak_entity();
        TextModal::open_single_line(
            &self.workspace,
            "Rename Changelist",
            IconName::ListTree,
            &current.clone(),
            window,
            cx,
            move |name, window, cx| {
                panel
                    .update(cx, |this, cx| {
                        let renamed = this.changelists.update(cx, |model| model.rename(id, &name));
                        if renamed == Some(false) && name != current.as_ref() {
                            this.show_error_toast(
                                "rename changelist",
                                anyhow::anyhow!("A changelist named \"{name}\" already exists"),
                                cx,
                            );
                        }
                        this.update_visible_entries(window, cx);
                        cx.notify();
                    })
                    .ok();
            },
        );
    }

    pub(in crate::git_panel) fn delete_changelist(
        &mut self,
        id: ChangelistId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.changelists.update(cx, |model| model.delete(id));
        self.collapsed_sections.remove(&Section::Changelist(id));
        self.update_visible_entries(window, cx);
        cx.notify();
    }

    fn deploy_changelist_menu(
        &mut self,
        position: Point<Pixels>,
        id: ChangelistId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let panel = cx.weak_entity();
        let menu = ContextMenu::build(window, cx, |menu, _, _| {
            menu.entry(
                "Rename Changelist…",
                None,
                panel_handler(&panel, move |this, window, cx| {
                    this.prompt_rename_changelist(id, window, cx)
                }),
            )
            .entry(
                "Delete Changelist",
                None,
                panel_handler(&panel, move |this, window, cx| {
                    this.delete_changelist(id, window, cx)
                }),
            )
            .separator()
            .entry(
                "New Changelist…",
                Some(NewChangelist.boxed_clone()),
                panel_handler(&panel, |this, window, cx| {
                    this.prompt_new_changelist(Vec::new(), window, cx)
                }),
            )
        });
        self.set_context_menu(menu, position, None, window, cx);
    }

    /// Adds "Move to Changelist" for the selected or marked files.
    pub(in crate::git_panel) fn changelist_submenu(
        &self,
        menu: ContextMenu,
        panel: PanelHandle,
    ) -> ContextMenu {
        if !self.changelists.is_active() {
            return menu;
        }
        let paths: Vec<RepoPath> = self
            .effective_status_entries()
            .into_iter()
            .map(|entry| entry.repo_path)
            .collect();
        let lists: Vec<_> = self
            .changelists
            .model()
            .lists()
            .iter()
            .map(|list| {
                let holds_all = paths
                    .iter()
                    .all(|path| self.changelists.is_in(path, list.id));
                (list.id, list.name.clone(), holds_all)
            })
            .collect();
        let any_in_list = paths.iter().any(|path| self.changelists.is_in_list(path));

        menu.separator()
            .submenu("Move to Changelist", move |mut submenu, _, _| {
                for (id, name, holds_all) in lists.clone() {
                    let paths = paths.clone();
                    submenu = submenu.toggleable_entry(
                        name,
                        holds_all,
                        IconPosition::Start,
                        None,
                        panel_handler(&panel, move |this, window, cx| {
                            this.assign_changes(paths.clone(), Some(id), window, cx)
                        }),
                    );
                }
                if any_in_list {
                    let paths = paths.clone();
                    submenu = submenu.entry(
                        "Remove from Changelist",
                        None,
                        panel_handler(&panel, move |this, window, cx| {
                            this.assign_changes(paths.clone(), None, window, cx)
                        }),
                    );
                }
                let paths = paths.clone();
                submenu.separator().entry(
                    "New Changelist…",
                    None,
                    panel_handler(&panel, move |this, window, cx| {
                        this.prompt_new_changelist(paths.clone(), window, cx)
                    }),
                )
            })
    }
}
