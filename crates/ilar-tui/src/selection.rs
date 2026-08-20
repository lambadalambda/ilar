//! Mouse selection over rendered transcript cells.
//!
//! Selection works on what is actually on screen, so it maps terminal
//! coordinates onto the rendered buffer rather than onto the transcript
//! model. Wide graphemes occupy several columns, so a cell records
//! whether it leads or continues one.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SelectionPoint {
    pub(crate) row: usize,
    pub(crate) column: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TranscriptSelection {
    pub(crate) anchor: SelectionPoint,
    pub(crate) focus: SelectionPoint,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RenderedCell {
    Character(char),
    Text(String),
    Space,
    Continuation { lead: usize },
}

pub(crate) type RenderedRow = Vec<RenderedCell>;

impl TranscriptSelection {
    fn ordered(self) -> (SelectionPoint, SelectionPoint) {
        if self.anchor <= self.focus {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        }
    }
}

pub(crate) fn selection_point(
    area: Rect,
    column: u16,
    row: u16,
    clamp: bool,
) -> Option<SelectionPoint> {
    if area.width == 0 || area.height == 0 {
        return None;
    }
    if !clamp && (column < area.x || column >= area.right() || row < area.y || row >= area.bottom())
    {
        return None;
    }
    let column = column.clamp(area.x, area.right().saturating_sub(1));
    let row = row.clamp(area.y, area.bottom().saturating_sub(1));
    Some(SelectionPoint {
        row: row.saturating_sub(area.y) as usize,
        column: column.saturating_sub(area.x) as usize,
    })
}

fn selected_columns(
    selection: TranscriptSelection,
    row: usize,
    width: usize,
) -> Option<std::ops::RangeInclusive<usize>> {
    if width == 0 || selection.anchor == selection.focus {
        return None;
    }
    let (start, end) = selection.ordered();
    if row < start.row || row > end.row {
        return None;
    }
    let first = if row == start.row { start.column } else { 0 }.min(width - 1);
    let last = if row == end.row {
        end.column
    } else {
        width - 1
    }
    .min(width - 1);
    (first <= last).then_some(first..=last)
}

fn grapheme_columns(
    row: &RenderedRow,
    columns: std::ops::RangeInclusive<usize>,
) -> std::ops::RangeInclusive<usize> {
    let mut first = *columns.start();
    let mut last = *columns.end();
    if let Some(RenderedCell::Continuation { lead }) = row.get(first) {
        first = *lead;
    }
    if let Some(RenderedCell::Continuation { lead }) = row.get(last) {
        last = *lead;
    }
    while matches!(row.get(last + 1), Some(RenderedCell::Continuation { lead }) if *lead == last) {
        last += 1;
    }
    first..=last
}

pub(crate) fn selected_transcript_text(
    rows: &[RenderedRow],
    selection: TranscriptSelection,
) -> Option<String> {
    if selection.anchor == selection.focus || rows.is_empty() {
        return None;
    }
    let (start, end) = selection.ordered();
    let last_row = end.row.min(rows.len().saturating_sub(1));
    if start.row > last_row {
        return None;
    }
    let selected = (start.row..=last_row)
        .map(|row| {
            let cells = &rows[row];
            selected_columns(selection, row, cells.len())
                .map(|columns| {
                    let mut text = String::new();
                    for column in grapheme_columns(cells, columns) {
                        match cells.get(column) {
                            Some(RenderedCell::Character(value)) => text.push(*value),
                            Some(RenderedCell::Text(value)) => text.push_str(value),
                            Some(RenderedCell::Space) => text.push(' '),
                            Some(RenderedCell::Continuation { .. }) | None => {}
                        }
                    }
                    text.trim_end_matches(' ').to_string()
                })
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!selected.is_empty()).then_some(selected)
}

pub(crate) fn selected_rows_unchanged(
    previous: &[RenderedRow],
    current: &[RenderedRow],
    selection: TranscriptSelection,
) -> bool {
    let (start, end) = selection.ordered();
    (start.row..=end.row).all(|row| previous.get(row) == current.get(row))
}

pub(crate) fn transcript_cells(buffer: &Buffer, area: Rect) -> Vec<RenderedRow> {
    (area.y..area.bottom())
        .map(|row| {
            let mut rendered = Vec::with_capacity(area.width as usize);
            let mut column = area.x;
            while column < area.right() {
                let symbol = buffer
                    .cell((column, row))
                    .map(|cell| cell.symbol())
                    .unwrap_or(" ");
                if symbol == " " {
                    rendered.push(RenderedCell::Space);
                    column += 1;
                    continue;
                }
                let lead = rendered.len();
                let width = UnicodeWidthStr::width(symbol)
                    .max(1)
                    .min(area.right().saturating_sub(column) as usize);
                let mut characters = symbol.chars();
                match (characters.next(), characters.next()) {
                    (Some(character), None) => {
                        rendered.push(RenderedCell::Character(character));
                    }
                    _ => rendered.push(RenderedCell::Text(symbol.to_string())),
                }
                for _ in 1..width {
                    rendered.push(RenderedCell::Continuation { lead });
                }
                column = column.saturating_add(width as u16);
            }
            rendered
        })
        .collect()
}

pub(crate) fn highlight_transcript_selection(
    buffer: &mut Buffer,
    area: Rect,
    selection: TranscriptSelection,
    rows: &[RenderedRow],
) {
    for row in 0..area.height as usize {
        let Some(rendered) = rows.get(row) else {
            continue;
        };
        let Some(columns) = selected_columns(selection, row, rendered.len()) else {
            continue;
        };
        for column in grapheme_columns(rendered, columns) {
            if let Some(cell) = buffer.cell_mut((
                area.x.saturating_add(column as u16),
                area.y.saturating_add(row as u16),
            )) {
                cell.set_style(Style::default().add_modifier(Modifier::REVERSED));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cells(rows: &[&str], width: usize) -> Vec<RenderedRow> {
        rows.iter()
            .map(|row| {
                row.chars()
                    .map(|character| match character {
                        ' ' => RenderedCell::Space,
                        _ => RenderedCell::Character(character),
                    })
                    .chain(std::iter::repeat(RenderedCell::Space))
                    .take(width)
                    .collect()
            })
            .collect()
    }

    #[test]
    fn transcript_selection_copies_multiline_text_in_display_order() {
        let rows = cells(&["abc", "wxyz"], 6);
        let forward = TranscriptSelection {
            anchor: SelectionPoint { row: 0, column: 1 },
            focus: SelectionPoint { row: 1, column: 2 },
        };
        let reverse = TranscriptSelection {
            anchor: forward.focus,
            focus: forward.anchor,
        };

        assert_eq!(
            selected_transcript_text(&rows, forward).as_deref(),
            Some("bc\nwxy")
        );
        assert_eq!(
            selected_transcript_text(&rows, reverse).as_deref(),
            Some("bc\nwxy")
        );
    }

    #[test]
    fn transcript_selection_ignores_clicks_and_trailing_viewport_padding() {
        let rows = cells(&["ilar hello"], 16);
        let click = TranscriptSelection {
            anchor: SelectionPoint { row: 0, column: 3 },
            focus: SelectionPoint { row: 0, column: 3 },
        };
        let drag = TranscriptSelection {
            anchor: SelectionPoint { row: 0, column: 5 },
            focus: SelectionPoint { row: 0, column: 15 },
        };

        assert_eq!(selected_transcript_text(&rows, click), None);
        assert_eq!(
            selected_transcript_text(&rows, drag).as_deref(),
            Some("hello")
        );
    }

    #[test]
    fn transcript_mouse_points_are_clamped_to_the_visible_text_area() {
        let area = Rect::new(10, 4, 8, 3);
        assert_eq!(
            selection_point(area, 12, 5, false),
            Some(SelectionPoint { row: 1, column: 2 })
        );
        assert_eq!(selection_point(area, 9, 5, false), None);
        assert_eq!(
            selection_point(area, 30, 20, true),
            Some(SelectionPoint { row: 2, column: 7 })
        );
    }

    #[test]
    fn transcript_selection_preserves_wide_graphemes_without_phantom_spaces() {
        let area = Rect::new(0, 0, 4, 1);
        let mut buffer = Buffer::empty(area);
        buffer.set_string(0, 0, "界B", Style::default());
        let rows = transcript_cells(&buffer, area);
        let selection = TranscriptSelection {
            anchor: SelectionPoint { row: 0, column: 1 },
            focus: SelectionPoint { row: 0, column: 2 },
        };

        assert_eq!(
            selected_transcript_text(&rows, selection).as_deref(),
            Some("界B")
        );
        highlight_transcript_selection(&mut buffer, area, selection, &rows);
        for column in 0..=2 {
            assert!(buffer[(column, 0)].modifier.contains(Modifier::REVERSED));
        }
    }

    #[test]
    fn transcript_selection_does_not_copy_vertical_viewport_padding() {
        let rows = cells(&["hello"], 8);
        let selection = TranscriptSelection {
            anchor: SelectionPoint { row: 0, column: 0 },
            focus: SelectionPoint { row: 4, column: 7 },
        };
        assert_eq!(
            selected_transcript_text(&rows, selection).as_deref(),
            Some("hello")
        );
    }

    #[test]
    fn transcript_selection_ignores_changes_outside_selected_rows() {
        let previous = cells(&["stable", "thinking one"], 16);
        let current = cells(&["stable", "thinking two"], 16);
        let stable_selection = TranscriptSelection {
            anchor: SelectionPoint { row: 0, column: 0 },
            focus: SelectionPoint { row: 0, column: 3 },
        };
        let volatile_selection = TranscriptSelection {
            anchor: SelectionPoint { row: 1, column: 0 },
            focus: SelectionPoint { row: 1, column: 3 },
        };

        assert!(selected_rows_unchanged(
            &previous,
            &current,
            stable_selection
        ));
        assert!(!selected_rows_unchanged(
            &previous,
            &current,
            volatile_selection
        ));
    }
}
