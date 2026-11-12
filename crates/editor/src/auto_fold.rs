//! Folds a file's import block when it opens, see the `auto_fold` language setting.
//!
//! The block is found in the syntax tree, so multi-line imports work and no language server
//! is needed. Languages are listed by name with the node kinds their grammar uses.

use crate::Editor;
use gpui::{Context, Window};
use language::{BufferSnapshot, language_settings::LanguageSettings};
use multi_buffer::MultiBufferOffset;
use settings::AutoFoldKind;
use std::ops::Range;
use text::Point;

fn import_node_kinds(language: &str) -> &'static [&'static str] {
    match language {
        "Rust" => &["use_declaration", "extern_crate_declaration"],
        "TypeScript" | "TSX" | "JavaScript" => &["import_statement"],
        "Python" => &[
            "import_statement",
            "import_from_statement",
            "future_import_statement",
        ],
        "Go" | "Java" => &["import_declaration"],
        "CSharp" => &["using_directive"],
        "C" | "C++" => &["preproc_include"],
        _ => &[],
    }
}

enum ImportBlock {
    /// The grammar has not produced a tree yet.
    NotParsed,
    NothingToFold,
    /// From the end of the first import's line to the end of the last import.
    Fold(Range<Point>),
}

/// The first run of import nodes among the top-level nodes. Comments inside the run belong
/// to it, a leading one (such as a license header) stays out.
fn find_import_block(snapshot: &BufferSnapshot) -> ImportBlock {
    let Some(layer) = snapshot.syntax_layers().next() else {
        return ImportBlock::NotParsed;
    };
    let kinds = import_node_kinds(layer.language.name().as_ref());
    if kinds.is_empty() {
        return ImportBlock::NothingToFold;
    }

    let mut rows: Option<Range<u32>> = None;
    let mut cursor = layer.node().walk();
    for node in layer.node().children(&mut cursor) {
        if kinds.contains(&node.kind()) {
            let start = node.start_position();
            let end = node.end_position();
            // A trailing newline can make a node end at the start of the next line.
            let end_row = if end.column == 0 && end.row > start.row {
                end.row - 1
            } else {
                end.row
            };
            let rows = rows.get_or_insert(start.row as u32..end_row as u32);
            rows.end = end_row as u32;
        } else if rows.is_some() && !node.kind().contains("comment") {
            break;
        }
    }

    match rows {
        Some(rows) if rows.end > rows.start => ImportBlock::Fold(
            Point::new(rows.start, snapshot.line_len(rows.start))
                ..Point::new(rows.end, snapshot.line_len(rows.end)),
        ),
        _ => ImportBlock::NothingToFold,
    }
}

/// The import block when `row` is its first line, so the gutter can offer to fold it.
pub(crate) fn import_block_starting_at(
    snapshot: &BufferSnapshot,
    row: u32,
) -> Option<Range<Point>> {
    let layer = snapshot.syntax_layers().next()?;
    let mut cursor = layer.node().walk();
    // Most rows are no import, so reject them before walking the whole block.
    cursor.goto_first_child_for_byte(snapshot.point_to_offset(Point::new(row, 0)))?;
    let node = cursor.node();
    let kinds = import_node_kinds(layer.language.name().as_ref());
    if node.start_position().row != row as usize || !kinds.contains(&node.kind()) {
        return None;
    }
    match find_import_block(snapshot) {
        ImportBlock::Fold(range) if range.start.row == row => Some(range),
        _ => None,
    }
}

impl Editor {
    /// Runs until the file has a parsed tree, then folds at most once per editor.
    pub(super) fn auto_fold_on_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.auto_fold_pending {
            return;
        }
        let Some(buffer) = self
            .buffer
            .read(cx)
            .as_singleton()
            .filter(|_| self.mode.is_full())
        else {
            self.auto_fold_pending = false;
            return;
        };
        // Until the language is known, the settings would be the language-less defaults.
        if buffer.read(cx).language().is_none() {
            return;
        }
        if !LanguageSettings::for_buffer(buffer.read(cx), cx)
            .auto_fold
            .contains(&AutoFoldKind::Imports)
        {
            self.auto_fold_pending = false;
            return;
        }

        let range = match find_import_block(&buffer.read(cx).snapshot()) {
            ImportBlock::NotParsed => return,
            ImportBlock::NothingToFold => {
                self.auto_fold_pending = false;
                return;
            }
            ImportBlock::Fold(range) => range,
        };
        self.auto_fold_pending = false;

        let display_snapshot = self.display_snapshot(cx);
        let already_folded = display_snapshot
            .folds_in_range(MultiBufferOffset(0)..display_snapshot.buffer_snapshot().len())
            .next()
            .is_some();
        let cursor_inside = self
            .selections
            .all::<Point>(&display_snapshot)
            .iter()
            .any(|selection| selection.start < range.end && selection.end > range.start);
        if !already_folded && !cursor_inside {
            self.fold_ranges(vec![range], false, window, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{editor_tests::init_test, test::editor_test_context::EditorTestContext};
    use gpui::{AppContext as _, TestAppContext};
    use language::rust_lang;
    use multi_buffer::MultiBufferRow;
    use settings::AutoFoldKind;
    use text::Point;

    const FILE: &str = "// header\nuse a;\nuse b::{\n    c,\n};\n// note\nuse d;\n\nfn main() {}\n";

    async fn open(cx: &mut TestAppContext, text: &str) -> EditorTestContext {
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state(text);
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(rust_lang()), cx));
        cx.run_until_parked();
        cx
    }

    fn display_text(cx: &mut EditorTestContext) -> String {
        cx.update_editor(|editor, _, cx| editor.display_text(cx))
    }

    #[test]
    fn languages_are_listed_under_the_names_zed_registers_them_by() {
        for name in [
            "Rust",
            "TypeScript",
            "TSX",
            "JavaScript",
            "Python",
            "Go",
            "Java",
            "CSharp",
            "C",
            "C++",
        ] {
            assert!(!super::import_node_kinds(name).is_empty(), "{name}");
        }
    }

    #[gpui::test]
    async fn folds_restored_while_the_editor_is_built_win_over_auto_fold(cx: &mut TestAppContext) {
        init_test(cx, |settings| {
            settings.defaults.auto_fold = Some(vec![AutoFoldKind::Imports])
        });
        let buffer = cx.new(|cx| {
            let mut buffer = language::Buffer::local(FILE, cx);
            buffer.set_language(Some(rust_lang()), cx);
            buffer
        });
        cx.run_until_parked();
        // Restoring an editor reads its saved folds right after constructing it.
        let (editor, cx) = cx.add_window_view(|window, cx| {
            let mut editor = crate::Editor::for_buffer(buffer.clone(), None, window, cx);
            editor.fold_ranges(
                vec![Point::new(8, 11)..Point::new(8, 12)],
                false,
                window,
                cx,
            );
            editor
        });
        cx.run_until_parked();

        let text = editor.update(cx, |editor, cx| editor.display_text(cx));
        assert!(text.contains("use b::{"), "{text}");
    }

    #[gpui::test]
    async fn the_import_block_is_folded_once_the_file_is_parsed(cx: &mut TestAppContext) {
        init_test(cx, |settings| {
            settings.defaults.auto_fold = Some(vec![AutoFoldKind::Imports])
        });
        let mut cx = open(cx, &format!("{FILE}ˇ")).await;
        assert_eq!(
            display_text(&mut cx),
            "// header\nuse a;⋯\n\nfn main() {}\n"
        );
    }

    #[gpui::test]
    async fn nothing_is_folded_unless_the_setting_asks_for_it(cx: &mut TestAppContext) {
        init_test(cx, |_| {});
        let mut cx = open(cx, &format!("{FILE}ˇ")).await;
        assert_eq!(display_text(&mut cx), FILE);
    }

    #[gpui::test]
    async fn a_cursor_inside_the_imports_keeps_them_open(cx: &mut TestAppContext) {
        init_test(cx, |settings| {
            settings.defaults.auto_fold = Some(vec![AutoFoldKind::Imports])
        });
        let mut cx = open(cx, &FILE.replace("    c,", "    cˇ,")).await;
        assert_eq!(display_text(&mut cx), FILE);
    }

    #[gpui::test]
    async fn a_single_import_has_nothing_to_fold(cx: &mut TestAppContext) {
        init_test(cx, |settings| {
            settings.defaults.auto_fold = Some(vec![AutoFoldKind::Imports])
        });
        let mut cx = open(cx, "use a;\n\nfn main() {}\nˇ").await;
        assert_eq!(display_text(&mut cx), "use a;\n\nfn main() {}\n");
    }

    #[gpui::test]
    async fn only_the_first_import_row_offers_to_fold_the_block(cx: &mut TestAppContext) {
        init_test(cx, |_| {});
        let mut cx = open(cx, &format!("{FILE}ˇ")).await;
        let creases = cx.update_editor(|editor, window, cx| {
            let snapshot = editor.snapshot(window, cx);
            (0..9)
                .map(|row| {
                    snapshot
                        .crease_for_buffer_row(MultiBufferRow(row))
                        .map(|crease| crease.range().clone())
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(creases[1], Some(Point::new(1, 6)..Point::new(6, 6)));
        // Row 2 is left out: its braced body is foldable by indentation anyway.
        for row in [0, 3, 4, 5, 6, 7] {
            assert_eq!(creases[row], None, "row {row}");
        }
    }

    #[gpui::test]
    async fn expanded_deleted_lines_above_the_imports_shift_the_range_with_them(
        cx: &mut TestAppContext,
    ) {
        init_test(cx, |_| {});
        let mut cx = open(cx, &format!("{FILE}ˇ")).await;
        cx.set_head_text(&format!("// removed\n{FILE}"));
        cx.dispatch_action(crate::ExpandAllDiffHunks);
        cx.run_until_parked();

        let creases = cx.update_editor(|editor, window, cx| {
            let snapshot = editor.snapshot(window, cx);
            (0..3)
                .map(|row| {
                    snapshot
                        .crease_for_buffer_row(MultiBufferRow(row))
                        .map(|crease| crease.range().clone())
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(creases[0], None, "the deleted line");
        assert_eq!(creases[1], None, "the header comment");
        assert_eq!(creases[2], Some(Point::new(2, 6)..Point::new(7, 6)));
    }

    #[gpui::test]
    async fn an_unfolded_import_block_can_be_folded_again(cx: &mut TestAppContext) {
        init_test(cx, |settings| {
            settings.defaults.auto_fold = Some(vec![AutoFoldKind::Imports])
        });
        let mut cx = open(cx, &format!("{FILE}ˇ")).await;
        cx.update_editor(|editor, window, cx| editor.unfold_at(MultiBufferRow(1), window, cx));
        assert_eq!(display_text(&mut cx), FILE);

        cx.update_editor(|editor, window, cx| editor.fold_at(MultiBufferRow(1), window, cx));
        assert_eq!(
            display_text(&mut cx),
            "// header\nuse a;⋯\n\nfn main() {}\n"
        );
    }

    #[gpui::test]
    async fn files_that_already_have_folds_are_left_alone(cx: &mut TestAppContext) {
        init_test(cx, |settings| {
            settings.defaults.auto_fold = Some(vec![AutoFoldKind::Imports])
        });
        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state(&format!("{FILE}ˇ"));
        cx.update_editor(|editor, window, cx| {
            editor.fold_ranges(
                vec![Point::new(8, 11)..Point::new(8, 12)],
                false,
                window,
                cx,
            );
        });
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(rust_lang()), cx));
        cx.run_until_parked();

        let text = display_text(&mut cx);
        assert!(text.contains("use b::{"), "{text}");
    }
}
