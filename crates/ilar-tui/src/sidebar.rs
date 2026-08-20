//! The right-hand sidebar: todos, and its narrow-terminal fallback.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::text::{
    Truncation, safe_text, styled_graphemes, styled_line, truncate_display, wrap_styled_line,
};
use crate::theme::{self, MUTED, RUNNING as TOOL_ACTIVE};

const TODO_SIDEBAR_MIN_WIDTH: u16 = 121;
const TODO_SIDEBAR_WIDTH: u16 = 42;
const TODO_SIDEBAR_MAX_ITEMS: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContentAreas {
    pub(crate) transcript: Rect,
    pub(crate) todos: Option<Rect>,
}

pub(crate) fn content_areas(area: Rect) -> ContentAreas {
    if area.width < TODO_SIDEBAR_MIN_WIDTH {
        return ContentAreas {
            transcript: area,
            todos: None,
        };
    }
    let areas = Layout::horizontal([Constraint::Min(3), Constraint::Length(TODO_SIDEBAR_WIDTH)])
        .split(area);
    ContentAreas {
        transcript: areas[0],
        todos: Some(areas[1]),
    }
}

pub(crate) struct TodoRenderSnapshot {
    items: Vec<ilar::todo::TodoItem>,
    hidden: usize,
}

pub(crate) fn todo_render_snapshot(list: &ilar::todo::TodoList, cap: usize) -> TodoRenderSnapshot {
    let indices = visible_todo_indices(list, cap);
    TodoRenderSnapshot {
        hidden: list.items.len().saturating_sub(indices.len()),
        items: indices
            .into_iter()
            .map(|index| list.items[index].clone())
            .collect(),
    }
}

pub(crate) fn todo_sidebar_snapshot(
    list: &ilar::todo::TodoList,
    height: usize,
) -> TodoRenderSnapshot {
    todo_render_snapshot(list, TODO_SIDEBAR_MAX_ITEMS.min(height))
}

#[cfg(test)]
fn render_todo_sidebar_lines(
    list: &ilar::todo::TodoList,
    width: u16,
    height: u16,
) -> Vec<Line<'static>> {
    let snapshot = todo_sidebar_snapshot(list, height as usize);
    render_todo_sidebar_snapshot(&snapshot, width, height)
}

pub(crate) fn render_todo_sidebar_snapshot(
    snapshot: &TodoRenderSnapshot,
    width: u16,
    height: u16,
) -> Vec<Line<'static>> {
    if width == 0 || height == 0 {
        return Vec::new();
    }
    if snapshot.items.is_empty() {
        return vec![Line::from(Span::styled(
            "— no todos",
            Style::default().fg(MUTED),
        ))];
    }
    let rendered = snapshot
        .items
        .iter()
        .map(|item| {
            let (marker, marker_style, content_style) = match item.status {
                ilar::todo::Status::Completed => (
                    "✓ ",
                    Style::default().fg(theme::SUCCESS),
                    Style::default().fg(theme::SECONDARY),
                ),
                ilar::todo::Status::InProgress => (
                    "▸ ",
                    Style::default().fg(theme::WAITING),
                    Style::default()
                        .fg(theme::PRIMARY)
                        .add_modifier(Modifier::BOLD),
                ),
                ilar::todo::Status::Pending => (
                    "○ ",
                    Style::default().fg(MUTED),
                    Style::default().fg(theme::SECONDARY),
                ),
            };
            let remaining = width as usize;
            let marker = truncate_display(marker, remaining, Truncation::Right);
            let remaining = remaining.saturating_sub(UnicodeWidthStr::width(marker.as_str()));
            let content = item
                .content
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let content = safe_text(&content);
            let body = Line::from(Span::styled(content, content_style));
            wrap_styled_line(body, remaining)
                .into_iter()
                .enumerate()
                .map(move |(line_index, mut body)| {
                    let prefix = if line_index == 0 {
                        Span::styled(marker.clone(), marker_style)
                    } else {
                        Span::raw(" ".repeat(UnicodeWidthStr::width(marker.as_str())))
                    };
                    let mut spans = vec![prefix];
                    spans.append(&mut body.spans);
                    Line::from(spans)
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    let height = height as usize;
    let mut output = Vec::new();
    let mut shown_items = 0usize;
    for rows in &rendered {
        if output.len().saturating_add(rows.len()) > height {
            break;
        }
        output.extend(rows.iter().cloned());
        shown_items += 1;
    }

    let mut partial_item = false;
    if shown_items == 0
        && let Some(rows) = rendered.first()
    {
        output.extend(rows.iter().take(height).cloned());
        partial_item = rows.len() > height;
        shown_items = 1;
    }

    let hidden = snapshot.hidden + rendered.len().saturating_sub(shown_items);
    if output.len() < height && (partial_item || hidden > 0) {
        let message = match (partial_item, hidden) {
            (true, 0) => "…".to_string(),
            (true, hidden) => format!("… · +{hidden} hidden"),
            (false, hidden) => format!("+{hidden} hidden"),
        };
        output.push(Line::from(Span::styled(
            truncate_display(&message, width as usize, Truncation::Right),
            Style::default().fg(MUTED),
        )));
    } else if partial_item || hidden > 0 {
        let message = match (partial_item, hidden) {
            (true, 0) => " · …".to_string(),
            (true, hidden) => format!(" · … · +{hidden} hidden"),
            (false, hidden) => format!(" · +{hidden} hidden"),
        };
        if let Some(last) = output.pop() {
            output.push(append_todo_note(last, width as usize, &message));
        }
    }
    output
}

fn append_todo_note(line: Line<'static>, width: usize, note: &str) -> Line<'static> {
    let note = truncate_display(note, width, Truncation::Right);
    let cells = styled_graphemes(line);
    let mut available = width.saturating_sub(UnicodeWidthStr::width(note.as_str()));
    let mut end = 0usize;
    let mut used = 0usize;
    while end < cells.len() && used.saturating_add(cells[end].width) <= available {
        used = used.saturating_add(cells[end].width);
        end += 1;
    }
    let shortened = end < cells.len() && !note.contains('…') && available > 0;
    if shortened {
        available -= 1;
        end = 0;
        used = 0;
        while end < cells.len() && used.saturating_add(cells[end].width) <= available {
            used = used.saturating_add(cells[end].width);
            end += 1;
        }
    }
    let mut line = styled_line(&cells[..end]);
    if shortened {
        line.spans
            .push(Span::styled("…", Style::default().fg(MUTED)));
    }
    line.spans
        .push(Span::styled(note, Style::default().fg(MUTED)));
    line
}

pub(crate) fn todo_summary(snapshot: &TodoRenderSnapshot, width: u16) -> Option<Line<'static>> {
    let item = snapshot.items.first()?;
    if width == 0 {
        return None;
    }
    let (marker, marker_style) = match item.status {
        ilar::todo::Status::Completed => ("✓ ", Style::default().fg(theme::SUCCESS)),
        ilar::todo::Status::InProgress => ("▸ ", Style::default().fg(TOOL_ACTIVE)),
        ilar::todo::Status::Pending => ("○ ", Style::default().fg(MUTED)),
    };
    let prefix = "todos ";
    let suffix = if snapshot.hidden > 0 {
        format!(" · +{}", snapshot.hidden)
    } else {
        String::new()
    };
    let fixed_width = UnicodeWidthStr::width(prefix)
        + UnicodeWidthStr::width(marker)
        + UnicodeWidthStr::width(suffix.as_str());
    let content = safe_text(
        &item
            .content
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
    );
    let content = truncate_display(
        &content,
        (width as usize).saturating_sub(fixed_width),
        Truncation::Right,
    );
    let text = format!("{prefix}{marker}{content}{suffix}");
    Some(Line::from(Span::styled(
        truncate_display(&text, width as usize, Truncation::Right),
        marker_style,
    )))
}

fn visible_todo_indices(list: &ilar::todo::TodoList, cap: usize) -> Vec<usize> {
    if cap == 0 {
        return Vec::new();
    }
    if list.items.len() <= cap {
        return (0..list.items.len()).collect();
    }
    let mut selected = std::collections::BTreeSet::new();
    if let Some(index) = list
        .items
        .iter()
        .position(|item| item.status == ilar::todo::Status::InProgress)
    {
        selected.insert(index);
    }
    if selected.len() < cap
        && let Some(index) = list
            .items
            .iter()
            .position(|item| item.status == ilar::todo::Status::Pending)
    {
        selected.insert(index);
    }
    if selected.len() < cap
        && let Some(index) = list
            .items
            .iter()
            .rposition(|item| item.status == ilar::todo::Status::Completed)
    {
        selected.insert(index);
    }
    for index in 0..list.items.len() {
        if selected.len() == cap {
            break;
        }
        selected.insert(index);
    }
    selected.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::tests::rendered_text;

    #[test]
    fn current_todos_render_all_statuses_and_live_replacements() {
        let todos = std::sync::Arc::new(std::sync::Mutex::new(ilar::todo::TodoList {
            items: vec![
                ilar::todo::TodoItem {
                    content: "done thing".into(),
                    status: ilar::todo::Status::Completed,
                },
                ilar::todo::TodoItem {
                    content: "active thing".into(),
                    status: ilar::todo::Status::InProgress,
                },
                ilar::todo::TodoItem {
                    content: "later thing".into(),
                    status: ilar::todo::Status::Pending,
                },
            ],
        }));
        let rendered = render_todo_sidebar_lines(&todos.lock().unwrap(), 26, 5)
            .iter()
            .map(rendered_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("✓ done thing"), "{rendered}");
        assert!(rendered.contains("▸ active thing"), "{rendered}");
        assert!(rendered.contains("○ later thing"), "{rendered}");
        assert!(
            render_todo_sidebar_lines(&todos.lock().unwrap(), 26, 5)[0]
                .spans
                .iter()
                .any(|span| span.content.contains("done thing")
                    && span.style.fg == Some(theme::SECONDARY))
        );

        todos.lock().unwrap().items = vec![ilar::todo::TodoItem {
            content: "replacement".into(),
            status: ilar::todo::Status::Pending,
        }];
        let replaced = render_todo_sidebar_lines(&todos.lock().unwrap(), 26, 5)
            .iter()
            .map(rendered_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(replaced.contains("○ replacement"), "{replaced}");
        assert!(!replaced.contains("done thing"), "{replaced}");
    }

    #[test]
    fn todo_rendering_is_bounded_and_preserves_each_present_status() {
        let list = ilar::todo::TodoList {
            items: vec![
                ilar::todo::TodoItem {
                    content: "old completed item".into(),
                    status: ilar::todo::Status::Completed,
                },
                ilar::todo::TodoItem {
                    content: "another completed item".into(),
                    status: ilar::todo::Status::Completed,
                },
                ilar::todo::TodoItem {
                    content: "current \u{1b} active\nitem".into(),
                    status: ilar::todo::Status::InProgress,
                },
                ilar::todo::TodoItem {
                    content: "next pending item".into(),
                    status: ilar::todo::Status::Pending,
                },
                ilar::todo::TodoItem {
                    content: "extra \u{1b} pending\nitem".into(),
                    status: ilar::todo::Status::Pending,
                },
            ],
        };
        let lines = render_todo_sidebar_lines(&list, 26, 3);
        let text = lines
            .iter()
            .map(rendered_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(lines.len(), 3);
        assert!(text.contains('✓'), "{text}");
        assert!(text.contains('▸'), "{text}");
        assert!(text.contains('○'), "{text}");
        assert!(text.contains("+2 hidden"), "{text}");
        assert!(!text.contains('\u{1b}'), "{text:?}");
        assert!(lines.iter().all(|line| !rendered_text(line).contains('\n')));
        assert!(lines.iter().all(|line| line.width() <= 26));
    }

    #[test]
    fn todo_sidebar_wraps_with_a_hanging_status_indent() {
        let list = ilar::todo::TodoList {
            items: vec![ilar::todo::TodoItem {
                content: "active task with readable wrapping".into(),
                status: ilar::todo::Status::InProgress,
            }],
        };

        let lines = render_todo_sidebar_lines(&list, 20, 4);
        let text = lines.iter().map(rendered_text).collect::<Vec<_>>();

        assert_eq!(text, ["▸ active task with", "  readable wrapping"]);
        assert!(lines.iter().all(|line| line.width() <= 20));
    }

    #[test]
    fn todo_sidebar_reports_items_displaced_by_wrapping() {
        let list = ilar::todo::TodoList {
            items: vec![
                ilar::todo::TodoItem {
                    content: "first todo needs multiple rows to render".into(),
                    status: ilar::todo::Status::InProgress,
                },
                ilar::todo::TodoItem {
                    content: "later todo".into(),
                    status: ilar::todo::Status::Pending,
                },
            ],
        };

        let lines = render_todo_sidebar_lines(&list, 20, 2);
        let text = lines.iter().map(rendered_text).collect::<Vec<_>>();

        assert_eq!(lines.len(), 2);
        assert!(text[0].starts_with("▸ first todo"), "{text:?}");
        assert!(text[1].contains("+1 hidden"), "{text:?}");
    }

    #[test]
    fn todo_sidebar_wraps_the_last_visible_item_before_hidden_count() {
        let mut items = (0..4)
            .map(|index| ilar::todo::TodoItem {
                content: format!("short todo {index}"),
                status: ilar::todo::Status::Pending,
            })
            .collect::<Vec<_>>();
        items.push(ilar::todo::TodoItem {
            content: "final selected todo wraps with visible ending".into(),
            status: ilar::todo::Status::Pending,
        });
        items.push(ilar::todo::TodoItem {
            content: "hidden todo".into(),
            status: ilar::todo::Status::Pending,
        });

        let lines = render_todo_sidebar_lines(&ilar::todo::TodoList { items }, 20, 10);
        let text = lines.iter().map(rendered_text).collect::<Vec<_>>();

        assert!(
            text.iter().any(|line| line.contains("visible ending")),
            "{text:?}"
        );
        assert!(
            text.iter().any(|line| line.contains("+1 hidden")),
            "{text:?}"
        );
    }

    #[test]
    fn inline_hidden_count_marks_shortened_todo_content() {
        let list = ilar::todo::TodoList {
            items: vec![
                ilar::todo::TodoItem {
                    content: "first".into(),
                    status: ilar::todo::Status::Pending,
                },
                ilar::todo::TodoItem {
                    content: "second".into(),
                    status: ilar::todo::Status::Pending,
                },
                ilar::todo::TodoItem {
                    content: "third item nearly".into(),
                    status: ilar::todo::Status::Pending,
                },
                ilar::todo::TodoItem {
                    content: "hidden".into(),
                    status: ilar::todo::Status::Pending,
                },
            ],
        };

        let lines = render_todo_sidebar_lines(&list, 20, 3);
        let last = rendered_text(lines.last().unwrap());

        assert!(last.contains("… · +1 hidden"), "{last}");
        assert!(lines.iter().all(|line| line.width() <= 20));
    }

    #[test]
    fn wide_content_reserves_a_fixed_right_sidebar() {
        let wide = content_areas(Rect::new(0, 0, 121, 8));
        assert_eq!(wide.transcript, Rect::new(0, 0, 79, 8));
        assert_eq!(wide.todos, Some(Rect::new(79, 0, 42, 8)));

        let narrow = content_areas(Rect::new(0, 0, 120, 8));
        assert_eq!(narrow.transcript, Rect::new(0, 0, 120, 8));
        assert_eq!(narrow.todos, None);
    }
}
