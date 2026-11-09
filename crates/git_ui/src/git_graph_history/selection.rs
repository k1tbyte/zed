use gpui::{App, Modifiers};
use std::{collections::BTreeSet, rc::Rc};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Pick {
    Single,
    Toggle,
    Range,
}

impl Pick {
    pub(crate) fn from_modifiers(modifiers: Modifiers, cx: &App) -> Self {
        if modifiers.shift {
            Pick::Range
        } else if modifiers.secondary(cx) {
            Pick::Toggle
        } else {
            Pick::Single
        }
    }
}

/// Rows picked with Cmd/Ctrl-click or Shift-click. Empty while the graph's own single
/// selection applies; when set, it holds every selected row, the graph's selected one included.
#[derive(Default)]
pub(crate) struct RowSelection {
    anchor: Option<usize>,
    rows: Rc<BTreeSet<usize>>,
}

/// What the render closures need to ask whether a row is selected.
#[derive(Clone)]
pub(crate) struct SelectedRows {
    primary: Option<usize>,
    rows: Rc<BTreeSet<usize>>,
}

impl SelectedRows {
    pub(crate) fn contains(&self, row: usize) -> bool {
        if self.rows.is_empty() {
            self.primary == Some(row)
        } else {
            self.rows.contains(&row)
        }
    }
}

impl RowSelection {
    /// Applies a click and returns the row the graph should show in its details panel.
    pub(crate) fn click(&mut self, row: usize, primary: Option<usize>, pick: Pick) -> usize {
        if pick == Pick::Range {
            let anchor = self.anchor.or(primary).unwrap_or(row);
            self.rows = Rc::new((anchor.min(row)..=anchor.max(row)).collect());
            self.anchor.get_or_insert(anchor);
            return row;
        }
        self.anchor = Some(row);
        if pick == Pick::Single {
            self.rows = Rc::default();
            return row;
        }

        let rows = Rc::make_mut(&mut self.rows);
        if rows.is_empty() {
            rows.extend(primary);
        }
        if !rows.remove(&row) {
            rows.insert(row);
        }
        let shown = match rows.last() {
            Some(last) if !rows.contains(&row) => *last,
            _ => row,
        };
        if rows.len() < 2 {
            rows.clear();
        }
        shown
    }

    pub(crate) fn clear(&mut self) {
        self.anchor = None;
        self.rows = Rc::default();
    }

    /// The selected rows when more than one is selected.
    pub(crate) fn multiple(&self) -> Option<&BTreeSet<usize>> {
        (self.rows.len() > 1).then_some(&*self.rows)
    }

    pub(crate) fn snapshot(&self, primary: Option<usize>) -> SelectedRows {
        SelectedRows {
            primary,
            rows: self.rows.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(selection: &RowSelection) -> Vec<usize> {
        selection
            .multiple()
            .map(|rows| rows.iter().copied().collect())
            .unwrap_or_default()
    }

    #[test]
    fn a_plain_click_leaves_the_single_selection_to_the_graph() {
        let mut selection = RowSelection::default();
        assert_eq!(selection.click(4, None, Pick::Single), 4);
        assert!(selection.multiple().is_none());
        assert!(selection.snapshot(Some(4)).contains(4));
        assert!(!selection.snapshot(Some(4)).contains(5));
    }

    #[test]
    fn toggling_adds_to_the_graph_selection_and_removes_again() {
        let mut selection = RowSelection::default();
        selection.click(2, Some(1), Pick::Toggle);
        selection.click(5, Some(2), Pick::Toggle);
        assert_eq!(rows(&selection), [1, 2, 5]);

        assert_eq!(selection.click(2, Some(5), Pick::Toggle), 5);
        assert_eq!(rows(&selection), [1, 5]);
        let snapshot = selection.snapshot(Some(5));
        assert!(snapshot.contains(1) && snapshot.contains(5) && !snapshot.contains(2));
    }

    #[test]
    fn toggling_down_to_one_row_shows_the_remaining_one_as_single() {
        let mut selection = RowSelection::default();
        selection.click(3, Some(1), Pick::Toggle);
        assert_eq!(selection.click(3, Some(3), Pick::Toggle), 1);
        assert!(selection.multiple().is_none());
    }

    #[test]
    fn a_range_runs_from_the_anchor_and_keeps_it() {
        let mut selection = RowSelection::default();
        selection.click(2, None, Pick::Single);
        selection.click(5, Some(2), Pick::Range);
        assert_eq!(rows(&selection), [2, 3, 4, 5]);
        selection.click(0, Some(5), Pick::Range);
        assert_eq!(rows(&selection), [0, 1, 2]);
    }

    #[test]
    fn a_range_starts_at_the_graph_selection_when_there_is_no_anchor() {
        let mut selection = RowSelection::default();
        selection.click(1, Some(3), Pick::Range);
        assert_eq!(rows(&selection), [1, 2, 3]);
    }

    #[test]
    fn a_plain_click_or_clear_drops_the_selection() {
        let mut selection = RowSelection::default();
        selection.click(4, Some(2), Pick::Range);
        selection.click(7, Some(4), Pick::Single);
        assert!(selection.multiple().is_none());
        selection.click(8, Some(7), Pick::Range);
        selection.clear();
        assert!(selection.multiple().is_none());
    }
}
