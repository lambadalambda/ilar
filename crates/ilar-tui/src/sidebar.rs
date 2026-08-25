//! The right-hand sidebar: the todo, agent and service panels, and its
//! narrow-terminal fallback. Rows and the geometry they land in;
//! view.rs frames and draws them.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::text::{
    Truncation, format_elapsed, safe_text, styled_graphemes, styled_line, truncate_display,
    wrap_styled_line,
};
use crate::theme::{self, MUTED, RUNNING as TOOL_ACTIVE};

const TODO_SIDEBAR_MIN_WIDTH: u16 = 121;
const TODO_SIDEBAR_WIDTH: u16 = 42;

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

/// Carve a bordered panel of `content_rows` lines off the top of
/// `area`, capped at half of it so stacked panels cannot crowd out the
/// todos below. Returns `None` — leaving `area` untouched — when the
/// cap leaves no room for any content.
pub(crate) fn carve_panel(area: &mut Rect, content_rows: usize) -> Option<Rect> {
    let height = (content_rows as u16 + 2).min(area.height / 2);
    if height <= 2 {
        return None;
    }
    let panel = Rect::new(area.x, area.y, area.width, height);
    *area = Rect::new(area.x, area.y + height, area.width, area.height - height);
    Some(panel)
}

/// One subagent working right now, as the sidebar shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentRow {
    pub(crate) description: String,
    pub(crate) agent: String,
    pub(crate) background: bool,
    pub(crate) elapsed: std::time::Duration,
}

/// How many agents the panel lists before counting the rest.
const AGENT_PANEL_MAX: usize = 3;

/// The running-agents panel: what was delegated, to whom, and for how
/// long. Two lines each — the description earns a full line, since it
/// is the only thing that says what the agent is actually doing.
pub(crate) fn agent_panel_lines(agents: &[AgentRow], width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for agent in agents.iter().take(AGENT_PANEL_MAX) {
        // Truncate the marker too: at absurd widths it is the overflow.
        let marker = truncate_display("▸ ", width, Truncation::Right);
        let remaining = width.saturating_sub(UnicodeWidthStr::width(marker.as_str()));
        lines.push(Line::from(vec![
            Span::styled(marker, Style::default().fg(TOOL_ACTIVE)),
            Span::styled(
                truncate_display(&safe_text(&agent.description), remaining, Truncation::Right),
                Style::default().fg(theme::PRIMARY),
            ),
        ]));
        let background = if agent.background { " · bg" } else { "" };
        lines.push(Line::styled(
            truncate_display(
                &format!(
                    "  {}{background} · {}",
                    safe_text(&agent.agent),
                    format_elapsed(agent.elapsed)
                ),
                width,
                Truncation::Right,
            ),
            Style::default().fg(MUTED),
        ));
    }
    if agents.len() > AGENT_PANEL_MAX {
        lines.push(Line::styled(
            truncate_display(
                &format!("  +{} more", agents.len() - AGENT_PANEL_MAX),
                width,
                Truncation::Right,
            ),
            Style::default().fg(MUTED),
        ));
    }
    lines
}

/// The services panel's rows, plus where its exited-services
/// disclosure landed among them — the one row that takes clicks.
pub(crate) struct ServicePanel {
    pub(crate) lines: Vec<Line<'static>>,
    pub(crate) exited_toggle: Option<usize>,
}

/// What runs is what matters: every running service, no cap
/// (`carve_panel` bounds by available space), and the dead collapsed to
/// a count so a crash still registers.
pub(crate) fn service_panel(
    services: &[(String, bool, String)],
    show_exited: bool,
    width: usize,
) -> ServicePanel {
    // Service names and details come from the process registry, which
    // takes them from user configuration and program output: sanitize
    // them like every other borrowed string that reaches a row.
    let row = |marker: &'static str, marker_color, name: &str, detail: &str, text_color| {
        Line::from(vec![
            Span::styled(marker, Style::default().fg(marker_color)),
            Span::styled(
                truncate_display(
                    &safe_text(&format!("{name} · {detail}")),
                    width.saturating_sub(2),
                    Truncation::Right,
                ),
                Style::default().fg(text_color),
            ),
        ])
    };
    let mut lines: Vec<Line<'static>> = services
        .iter()
        .filter(|(_, running, _)| *running)
        .map(|(name, _, detail)| row("● ", theme::SUCCESS, name, detail, theme::PRIMARY))
        .collect();
    let exited: Vec<_> = services.iter().filter(|(_, running, _)| !running).collect();
    let mut exited_toggle = None;
    if !exited.is_empty() {
        exited_toggle = Some(lines.len());
        let marker = if show_exited { "▾ " } else { "▸ " };
        lines.push(Line::from(vec![
            Span::styled(marker, Style::default().fg(MUTED)),
            Span::styled(
                format!("{} exited", exited.len()),
                Style::default().fg(MUTED),
            ),
        ]));
        if show_exited {
            for (name, _, detail) in &exited {
                lines.push(row("○ ", MUTED, name, detail, MUTED));
            }
        }
    }
    ServicePanel {
        lines,
        exited_toggle,
    }
}

/// Where the disclosure row landed on screen inside its carved panel —
/// `None` when the panel had no room to draw it.
pub(crate) fn exited_disclosure_hit(panel: Rect, index: usize) -> Option<Rect> {
    let row = panel.y + 1 + index as u16;
    (row < panel.bottom().saturating_sub(1))
        .then(|| Rect::new(panel.x + 1, row, panel.width.saturating_sub(2), 1))
}

/// Mark a row as the clickable the pointer is over.
pub(crate) fn underline_row(line: &mut Line<'static>) {
    for span in &mut line.spans {
        span.style = span.style.add_modifier(Modifier::UNDERLINED);
    }
}

pub(crate) struct TodoRenderSnapshot {
    items: Vec<ilar::todo::TodoItem>,
    /// Position in `items` of the one that must stay on screen.
    anchor: usize,
    hidden: usize,
}

/// Take the run of at most `cap` todos that keeps the active item on
/// screen. The cap is what the panel has room for, not a fixed number:
/// a tall sidebar shows the whole plan.
pub(crate) fn todo_render_snapshot(list: &ilar::todo::TodoList, cap: usize) -> TodoRenderSnapshot {
    let window = visible_todo_window(list, cap);
    TodoRenderSnapshot {
        hidden: list.items.len().saturating_sub(window.len()),
        anchor: anchor_index(list).saturating_sub(window.start),
        items: list.items[window].to_vec(),
    }
}

#[cfg(test)]
fn render_todo_sidebar_lines(
    list: &ilar::todo::TodoList,
    width: u16,
    height: u16,
) -> Vec<Line<'static>> {
    let snapshot = todo_render_snapshot(list, height as usize);
    render_todo_sidebar_snapshot(&snapshot, width, height)
}

/// One todo as the rows it needs at `width`: a status marker, then the
/// content wrapped under a hanging indent. The sidebar and the overlay
/// both draw todos this way, so a status only ever looks one way.
pub(crate) fn todo_item_lines(item: &ilar::todo::TodoItem, width: u16) -> Vec<Line<'static>> {
    let (marker, marker_style, content_style) = match item.status {
        ilar::todo::Status::Completed => (
            "✓ ",
            Style::default().fg(theme::SUCCESS),
            Style::default().fg(theme::SECONDARY),
        ),
        ilar::todo::Status::InProgress => (
            "▸ ",
            // Same colour as todo_summary: in progress is active
            // work, not a wait state.
            Style::default().fg(TOOL_ACTIVE),
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
        .collect()
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
        .map(|item| todo_item_lines(item, width))
        .collect::<Vec<_>>();

    let height = height as usize;
    let anchor = snapshot.anchor.min(rendered.len().saturating_sub(1));
    let costs = rendered.iter().map(Vec::len).collect::<Vec<_>>();
    let run = fitting_run(&costs, anchor, height);
    let mut output = rendered[run.clone()]
        .iter()
        .flatten()
        .cloned()
        .collect::<Vec<_>>();
    let mut shown_items = run.len();

    let mut partial_item = false;
    if shown_items == 0 {
        let rows = &rendered[anchor];
        output.extend(rows.iter().take(height).cloned());
        partial_item = rows.len() > height;
        shown_items = 1;
    }

    let hidden = snapshot.hidden + rendered.len().saturating_sub(shown_items);
    // Hiding is only fair if the way to see the rest is on screen. The
    // hint costs five cells, so a cramped panel goes without.
    let hint = if width >= 30 { " · ^T" } else { "" };
    if output.len() < height && (partial_item || hidden > 0) {
        let message = match (partial_item, hidden) {
            (true, 0) => format!("…{hint}"),
            (true, hidden) => format!("… · +{hidden} hidden{hint}"),
            (false, hidden) => format!("+{hidden} hidden{hint}"),
        };
        output.push(Line::from(Span::styled(
            truncate_display(&message, width as usize, Truncation::Right),
            Style::default().fg(MUTED),
        )));
    } else if partial_item || hidden > 0 {
        let message = match (partial_item, hidden) {
            (true, 0) => format!(" · …{hint}"),
            (true, hidden) => format!(" · … · +{hidden} hidden{hint}"),
            (false, hidden) => format!(" · +{hidden} hidden{hint}"),
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
    let item = snapshot.items.get(snapshot.anchor)?;
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

/// The todo the panel refuses to hide: work in progress, else the next
/// thing to start, else the most recent completion.
fn anchor_index(list: &ilar::todo::TodoList) -> usize {
    let first = |status| list.items.iter().position(|item| item.status == status);
    first(ilar::todo::Status::InProgress)
        .or_else(|| first(ilar::todo::Status::Pending))
        .or_else(|| {
            list.items
                .iter()
                .rposition(|item| item.status == ilar::todo::Status::Completed)
        })
        .unwrap_or(0)
}

/// The contiguous run of at most `cap` todos containing the anchor,
/// which sits at the top of it: when the list outgrows the panel, the
/// work still ahead is worth more than the work already done. A run
/// stays contiguous on purpose — a pick of items 1, 2 and 17 reads as
/// a list with silent gaps.
fn visible_todo_window(list: &ilar::todo::TodoList, cap: usize) -> std::ops::Range<usize> {
    let len = list.items.len();
    let cap = cap.min(len);
    if cap == 0 {
        return 0..0;
    }
    // Back-fill with earlier items when the anchor is near the end.
    let end = len.min(anchor_index(list) + cap);
    end - cap..end
}

/// The run of rendered items that fits in `height` rows and contains
/// `anchor`, growing forward before backward. Empty when even the
/// anchor alone overflows — the caller then clips it.
fn fitting_run(costs: &[usize], anchor: usize, height: usize) -> std::ops::Range<usize> {
    if costs.is_empty() || costs[anchor] > height {
        return anchor..anchor;
    }
    let mut used = costs[anchor];
    let mut end = anchor + 1;
    while end < costs.len() && used + costs[end] <= height {
        used += costs[end];
        end += 1;
    }
    let mut start = anchor;
    while start > 0 && used + costs[start - 1] <= height {
        used += costs[start - 1];
        start -= 1;
    }
    start..end
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::tests::rendered_text;

    #[test]
    fn carve_panel_takes_the_top_and_respects_the_half_height_cap() {
        let mut area = Rect::new(0, 10, 42, 20);
        let panel = carve_panel(&mut area, 3).expect("fits");
        assert_eq!(panel, Rect::new(0, 10, 42, 5));
        assert_eq!(area, Rect::new(0, 15, 42, 15));

        // Tall content is capped at half the remaining area.
        let panel = carve_panel(&mut area, 40).expect("capped");
        assert_eq!(panel.height, 7);
        assert_eq!(area, Rect::new(0, 22, 42, 8));

        // Too small to show any content: nothing carved, area untouched.
        let mut tiny = Rect::new(0, 0, 42, 4);
        assert_eq!(carve_panel(&mut tiny, 3), None);
        assert_eq!(tiny, Rect::new(0, 0, 42, 4));
    }

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
    fn todo_rendering_is_bounded_and_starts_at_the_active_item() {
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
        // Three rows for five items: the completed pair above the
        // active one is what goes, not the pending work below it.
        assert!(!text.contains('✓'), "{text}");
        assert!(text.starts_with('▸'), "{text}");
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
            content: "hidden todo that would need rows of its own".into(),
            status: ilar::todo::Status::Pending,
        });

        // Eight rows hold the four short items and the wrapping one;
        // the last item needs more than the row left over.
        let lines = render_todo_sidebar_lines(&ilar::todo::TodoList { items }, 20, 8);
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

    fn list_of(statuses: &[ilar::todo::Status]) -> ilar::todo::TodoList {
        ilar::todo::TodoList {
            items: statuses
                .iter()
                .enumerate()
                .map(|(index, status)| ilar::todo::TodoItem {
                    content: format!("item {index}"),
                    status: *status,
                })
                .collect(),
        }
    }

    #[test]
    fn the_agent_panel_caps_its_rows_and_counts_the_rest() {
        let agents = (0..5)
            .map(|index| AgentRow {
                description: format!("task number {index} with a long description"),
                agent: "explore".into(),
                background: index % 2 == 1,
                elapsed: std::time::Duration::from_secs(30),
            })
            .collect::<Vec<_>>();

        let lines = agent_panel_lines(&agents, 24);
        let text = lines.iter().map(rendered_text).collect::<Vec<_>>();

        // Three agents, two lines each, then the tally.
        assert_eq!(text.len(), 7, "{text:?}");
        assert!(text[0].starts_with("▸ task number 0"), "{text:?}");
        assert_eq!(text[1], "  explore · 30s");
        assert_eq!(text[3], "  explore · bg · 30s");
        assert_eq!(text[6], "  +2 more");
        assert!(lines.iter().all(|line| line.width() <= 24), "{text:?}");

        // A single agent needs no tally, and zero width does not panic.
        assert_eq!(agent_panel_lines(&agents[..1], 24).len(), 2);
        assert!(
            agent_panel_lines(&agents, 0)
                .iter()
                .all(|line| line.width() <= 1)
        );
    }

    #[test]
    fn the_window_holds_the_anchor_and_stays_contiguous() {
        use ilar::todo::Status::{Completed, InProgress, Pending};

        let list = list_of(&[Completed, Completed, InProgress, Pending, Pending]);
        assert_eq!(visible_todo_window(&list, 0), 0..0);
        assert_eq!(anchor_index(&list), 2);
        // Room for everything: the whole list, from the top.
        assert_eq!(visible_todo_window(&list, 9), 0..5);
        // Tight: the active item leads, upcoming work follows.
        assert_eq!(visible_todo_window(&list, 2), 2..4);

        // Nothing in progress: the next pending item anchors.
        let list = list_of(&[Completed, Completed, Completed, Pending, Pending]);
        assert_eq!(visible_todo_window(&list, 2), 3..5);
        // All done: the tail, back-filled because the anchor is last.
        let list = list_of(&[Completed, Completed, Completed]);
        assert_eq!(visible_todo_window(&list, 2), 1..3);
    }

    #[test]
    fn the_fitting_run_grows_forward_then_backward() {
        // Anchor at 2, four rows: forward first, then back-fill.
        assert_eq!(fitting_run(&[1, 1, 1, 1, 1], 2, 4), 1..5);
        // Wrapping ahead eats the budget; nothing back-fills.
        assert_eq!(fitting_run(&[1, 1, 3, 1], 2, 4), 2..4);
        // Not even the anchor fits: the caller clips it.
        assert_eq!(fitting_run(&[1, 5, 1], 1, 3), 1..1);
    }

    #[test]
    fn a_tall_sidebar_shows_every_todo_it_has_room_for() {
        let list = ilar::todo::TodoList {
            items: (0..12)
                .map(|index| ilar::todo::TodoItem {
                    content: format!("todo {index}"),
                    status: ilar::todo::Status::Pending,
                })
                .collect(),
        };

        let lines = render_todo_sidebar_lines(&list, 26, 30);
        let text = lines.iter().map(rendered_text).collect::<Vec<_>>();

        assert_eq!(text.len(), 12, "{text:?}");
        for index in 0..12 {
            assert!(
                text.iter()
                    .any(|line| line.contains(&format!("todo {index}"))),
                "todo {index} missing: {text:?}"
            );
        }
        assert!(!text.join("\n").contains("hidden"), "{text:?}");
    }

    #[test]
    fn a_short_sidebar_keeps_the_active_item_and_what_follows() {
        let mut items = (0..15)
            .map(|index| ilar::todo::TodoItem {
                content: format!("done {index}"),
                status: ilar::todo::Status::Completed,
            })
            .collect::<Vec<_>>();
        items.push(ilar::todo::TodoItem {
            content: "active one".into(),
            status: ilar::todo::Status::InProgress,
        });
        items.extend((0..4).map(|index| ilar::todo::TodoItem {
            content: format!("next {index}"),
            status: ilar::todo::Status::Pending,
        }));

        let list = ilar::todo::TodoList { items };
        let lines = render_todo_sidebar_lines(&list, 38, 4);
        let text = lines.iter().map(rendered_text).collect::<Vec<_>>();

        assert_eq!(text.len(), 4, "{text:?}");
        assert!(text[0].contains("▸ active one"), "{text:?}");
        assert!(text[1].contains("next 0"), "{text:?}");
        assert!(text[2].contains("next 1"), "{text:?}");
        // 15 completed above and one pending below the window, named
        // alongside the key that shows them.
        assert!(text[3].contains("+16 hidden · ^T"), "{text:?}");

        // Too narrow for the hint: the count still has to fit.
        let cramped = render_todo_sidebar_lines(&list, 20, 4)
            .iter()
            .map(rendered_text)
            .collect::<Vec<_>>();
        assert!(cramped[3].contains("+16 hidden"), "{cramped:?}");
        assert!(!cramped[3].contains("^T"), "{cramped:?}");
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
