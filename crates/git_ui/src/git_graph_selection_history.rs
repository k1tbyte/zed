use buffer_diff::BufferDiffSnapshot;
use git::repository::RepoPath;
use gpui::{Context, Entity};
use language::ToPoint;
use project::git_store::RepositoryId;
use std::ops::Range;
use workspace::{Toast, Workspace, notifications::NotificationId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SelectionHistoryProjection {
    Mapped { start_line: u32, end_line: u32 },
    NoCommittedLines,
}

#[derive(Clone)]
pub(crate) enum PendingSelectionHistoryTarget {
    Ready {
        repo_id: RepositoryId,
        repo_path: RepoPath,
        buffer: Entity<language::Buffer>,
        source_range: Range<language::Anchor>,
    },
    UnsupportedDeletedSelection,
}

pub(crate) enum SelectionHistoryError {
    NoCommittedLines,
    UnsupportedDeletedSelection,
    DiffLoad(String),
}

impl SelectionHistoryError {
    pub(crate) fn notification_id(&self) -> NotificationId {
        let id = match self {
            Self::NoCommittedLines => "selection-history-no-committed-lines",
            Self::UnsupportedDeletedSelection => "selection-history-deleted-lines",
            Self::DiffLoad(_) => "selection-history-diff-load-failed",
        };
        NotificationId::named(id.into())
    }

    pub(crate) fn message(&self) -> String {
        match self {
            Self::NoCommittedLines => "Selected lines have no commit history".to_string(),
            Self::UnsupportedDeletedSelection => {
                "Selection endpoints inside deleted lines are not supported".to_string()
            }
            Self::DiffLoad(error) => format!("Failed to load selection history: {error}"),
        }
    }
}

pub(crate) fn show_selection_history_error(
    workspace: &mut Workspace,
    error: SelectionHistoryError,
    cx: &mut Context<Workspace>,
) {
    workspace.show_toast(Toast::new(error.notification_id(), error.message()), cx);
}

fn validate_selection_history_projection(
    start_line: u32,
    end_line: u32,
    base_text: &language::BufferSnapshot,
) -> SelectionHistoryProjection {
    let max_point = base_text.max_point();
    let committed_line_count = if base_text.is_empty() {
        0
    } else if max_point.column == 0 {
        max_point.row
    } else {
        max_point.row + 1
    };

    if start_line > committed_line_count {
        SelectionHistoryProjection::NoCommittedLines
    } else {
        SelectionHistoryProjection::Mapped {
            start_line,
            end_line,
        }
    }
}

pub(crate) fn project_selection_history_to_head(
    source_range: Range<language::Anchor>,
    buffer: &language::BufferSnapshot,
    diff: &BufferDiffSnapshot,
) -> SelectionHistoryProjection {
    let start = source_range.start.to_point(buffer);
    let end = source_range.end.to_point(buffer);
    let start_row = start.row;
    let end_row = if end.column == 0 && end.row > start_row {
        end.row - 1
    } else {
        end.row
    };

    let normalized_start = language::Point::new(start_row, 0);
    let normalized_end = language::Point::new(end_row, buffer.line_len(end_row));
    let normalized_range =
        buffer.anchor_before(normalized_start)..buffer.anchor_after(normalized_end);

    let hunks = diff
        .raw_hunks_intersecting_range(normalized_range, buffer)
        .collect::<Vec<_>>();
    let mut fully_added_rows = hunks
        .iter()
        .filter(|hunk| hunk.diff_base_byte_range.is_empty() && !hunk.range.is_empty())
        .filter_map(|hunk| {
            let start_row = if hunk.range.start.column == 0 {
                hunk.range.start.row
            } else {
                hunk.range.start.row + 1
            };
            let end_row = if hunk.range.end.column == 0 {
                hunk.range.end.row
            } else if hunk.range.end.column >= buffer.line_len(hunk.range.end.row) {
                hunk.range.end.row + 1
            } else {
                hunk.range.end.row
            };
            (start_row < end_row).then_some(start_row..end_row)
        })
        .collect::<Vec<_>>();
    fully_added_rows.sort_by_key(|range| range.start);

    let mut first_committed_row = start_row;
    for added_rows in &fully_added_rows {
        if added_rows.end <= first_committed_row {
            continue;
        }
        if added_rows.start > first_committed_row {
            break;
        }
        first_committed_row = added_rows.end;
    }
    if first_committed_row > end_row {
        return SelectionHistoryProjection::NoCommittedLines;
    }

    let base_text = diff.base_text();
    let mut base_start = diff.buffer_point_to_base_text_point(normalized_start, buffer);
    let mut base_end = diff.buffer_point_to_base_text_point(normalized_end, buffer);
    if base_end < base_start {
        std::mem::swap(&mut base_start, &mut base_end);
    }

    for hunk in hunks {
        if hunk.range.is_empty() || hunk.diff_base_byte_range.is_empty() {
            continue;
        }
        let intersects = if normalized_start == normalized_end {
            hunk.range.start <= normalized_start && hunk.range.end >= normalized_end
        } else {
            hunk.range.start < normalized_end && hunk.range.end > normalized_start
        };
        if !intersects {
            continue;
        }
        base_start = base_start.min(base_text.offset_to_point(hunk.diff_base_byte_range.start));
        base_end = base_end.max(base_text.offset_to_point(hunk.diff_base_byte_range.end));
    }

    if base_start == base_end {
        let point = diff
            .buffer_point_to_base_text_point(language::Point::new(first_committed_row, 0), buffer);
        let line = point.row + 1;
        return validate_selection_history_projection(line, line, base_text);
    }

    let start_line = base_start.row + 1;
    let end_line = if base_end.column == 0 && base_end.row > base_start.row {
        base_end.row
    } else {
        base_end.row + 1
    };
    validate_selection_history_projection(start_line, end_line, base_text)
}
