//! JetBrains-like smart backspace: with only whitespace before the cursor, it joins the line
//! to the end of the previous one in a single keystroke.

use crate::{Backspace, Editor};
use gpui::{Context, Window, actions};
use multi_buffer::{MultiBufferPoint, MultiBufferRow};
use text::SelectionGoal;

actions!(
    editor,
    [
        /// Deletes the whitespace before the cursor together with the preceding line break
        /// when nothing else precedes the cursor on its line, otherwise acts as backspace.
        HungryBackspace
    ]
);

impl Editor {
    pub fn hungry_backspace(
        &mut self,
        _: &HungryBackspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only(cx) {
            return;
        }
        self.transact(window, cx, |this, window, cx| {
            let display_map = this.display_map.update(cx, |map, cx| map.snapshot(cx));
            let buffer = display_map.buffer_snapshot();
            let mut selections = this.selections.all::<MultiBufferPoint>(&display_map);
            for selection in &mut selections {
                let head = selection.head();
                if selection.is_empty()
                    && head.row > 0
                    && head.column > 0
                    && buffer.is_line_whitespace_upto(head)
                {
                    let row = head.row - 1;
                    let previous_line_end =
                        MultiBufferPoint::new(row, buffer.line_len(MultiBufferRow(row)));
                    selection.set_head(previous_line_end, SelectionGoal::None);
                }
            }
            this.change_selections(Default::default(), window, cx, |s| s.select(selections));
            this.backspace(&Backspace, window, cx);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::HungryBackspace;
    use crate::{editor_tests::init_test, test::editor_test_context::EditorTestContext};
    use gpui::TestAppContext;
    use indoc::indoc;
    use text::Point;

    fn assert_hungry_backspace(cx: &mut EditorTestContext, before: &str, after: &str) {
        cx.set_state(before);
        cx.update_editor(|editor, window, cx| {
            editor.hungry_backspace(&HungryBackspace, window, cx)
        });
        cx.assert_editor_state(after);
    }

    #[gpui::test]
    async fn joins_lines_when_only_whitespace_precedes_the_cursor(cx: &mut TestAppContext) {
        init_test(cx, |_| {});
        let mut cx = EditorTestContext::new(cx).await;

        assert_hungry_backspace(&mut cx, "    foo();\n        ˇ\n}", "    foo();ˇ\n}");
        assert_hungry_backspace(&mut cx, "    foo();\n\n    ˇbar();", "    foo();\nˇbar();");
        assert_hungry_backspace(&mut cx, "    foo();\n    ˇbar();", "    foo();ˇbar();");
        assert_hungry_backspace(
            &mut cx,
            indoc! {"
                one
                    ˇ
                two
                  ˇ
            "},
            indoc! {"
                oneˇ
                twoˇ
            "},
        );
    }

    #[gpui::test]
    async fn falls_back_to_backspace(cx: &mut TestAppContext) {
        init_test(cx, |_| {});
        let mut cx = EditorTestContext::new(cx).await;

        assert_hungry_backspace(&mut cx, "a\n    fooˇ", "a\n    foˇ");
        assert_hungry_backspace(&mut cx, "        ˇfoo", "    ˇfoo");
        assert_hungry_backspace(&mut cx, "a\n  «fooˇ»", "a\n  ˇ");

        cx.set_state("fn f() {\n    a();\n    ˇ}");
        cx.update_editor(|editor, window, cx| {
            editor.fold_ranges(vec![Point::new(0, 8)..Point::new(2, 4)], false, window, cx);
            editor.hungry_backspace(&HungryBackspace, window, cx);
        });
        cx.assert_editor_state("fn f() {ˇ}");
    }

    #[gpui::test]
    async fn undoes_in_one_step(cx: &mut TestAppContext) {
        init_test(cx, |_| {});
        let mut cx = EditorTestContext::new(cx).await;

        assert_hungry_backspace(&mut cx, "foo\n    ˇ", "fooˇ");
        cx.update_editor(|editor, window, cx| editor.undo(&crate::Undo, window, cx));
        cx.assert_editor_state("foo\n    ˇ");
    }
}
