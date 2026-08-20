//! Text measurement, truncation, wrapping and value formatting.
//!
//! Pure helpers over strings and styled lines: no app state, no I/O.

use crate::theme;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub(crate) fn format_cost(cost: f64) -> String {
    if cost >= 0.995 {
        format!("${cost:.2}")
    } else if cost >= 0.0005 {
        format!("${cost:.3}")
    } else if cost > 0.0 {
        "$<0.001".into()
    } else {
        "$0.00".into()
    }
}

#[derive(Clone)]
pub(crate) struct StyledGrapheme {
    text: String,
    style: Style,
    pub(crate) width: usize,
}

pub(crate) fn wrap_markdown_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let preformatted = line
        .spans
        .first()
        .is_some_and(|span| span.content == "│ " && span.style.fg == Some(theme::CODE));
    if preformatted {
        hard_wrap_styled_line(line, width)
    } else {
        wrap_styled_line(line, width)
    }
}

fn hard_wrap_styled_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return vec![Line::default()];
    }
    if line.width() <= width {
        return vec![line];
    }
    let mut output = Vec::new();
    let mut current = Vec::new();
    let mut current_width = 0usize;
    for mut cell in styled_graphemes(line) {
        if cell.width > width {
            cell.text = "…".into();
            cell.width = 1;
        }
        if !current.is_empty() && current_width.saturating_add(cell.width) > width {
            output.push(styled_line(&current));
            current.clear();
            current_width = 0;
        }
        current_width = current_width.saturating_add(cell.width);
        current.push(cell);
    }
    if !current.is_empty() {
        output.push(styled_line(&current));
    }
    output
}

pub(crate) fn wrap_styled_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return vec![Line::default()];
    }
    if line.width() <= width {
        return vec![line];
    }

    let cells = styled_graphemes(line)
        .into_iter()
        .map(|mut cell| {
            if cell.width > width {
                cell.text = "…".into();
                cell.width = 1;
            }
            cell
        })
        .collect::<Vec<_>>();
    if cells.is_empty() {
        return vec![Line::default()];
    }

    let mut output = Vec::new();
    let mut start = 0usize;
    while start < cells.len() {
        let mut end = start;
        let mut row_width = 0usize;
        while end < cells.len() && row_width.saturating_add(cells[end].width) <= width {
            row_width = row_width.saturating_add(cells[end].width);
            end += 1;
        }
        if end == cells.len() {
            output.push(styled_line(&cells[start..]));
            break;
        }
        if end == start {
            output.push(styled_line(&cells[start..start + 1]));
            start += 1;
            continue;
        }

        if cells[end].text.chars().all(char::is_whitespace) {
            output.push(styled_line(&cells[start..end]));
            start = end + 1;
            while start < cells.len() && cells[start].text.chars().all(char::is_whitespace) {
                start += 1;
            }
            continue;
        }

        let first_content =
            (start..end).find(|index| !cells[*index].text.chars().all(char::is_whitespace));
        let word_break = first_content.and_then(|first_content| {
            (first_content..end)
                .rev()
                .find(|index| cells[*index].text.chars().all(char::is_whitespace))
        });
        if let Some(word_break) = word_break {
            output.push(styled_line(&cells[start..word_break]));
            start = word_break + 1;
            while start < cells.len() && cells[start].text.chars().all(char::is_whitespace) {
                start += 1;
            }
        } else {
            output.push(styled_line(&cells[start..end]));
            start = end;
        }
    }
    output
}

pub(crate) fn styled_graphemes(line: Line<'static>) -> Vec<StyledGrapheme> {
    line.spans
        .into_iter()
        .flat_map(|span| {
            let style = span.style;
            UnicodeSegmentation::graphemes(span.content.as_ref(), true)
                .map(move |text| StyledGrapheme {
                    text: text.to_string(),
                    style,
                    width: UnicodeWidthStr::width(text),
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

pub(crate) fn styled_line(cells: &[StyledGrapheme]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for cell in cells {
        if let Some(last) = spans.last_mut()
            && last.style == cell.style
        {
            last.content.to_mut().push_str(&cell.text);
        } else {
            spans.push(Span::styled(cell.text.clone(), cell.style));
        }
    }
    Line::from(spans)
}

pub(crate) fn text_field_view(value: &str, width: u16) -> (String, u16) {
    text_field_view_at(value, value.len(), width)
}

pub(crate) fn text_field_view_at(value: &str, cursor: usize, width: u16) -> (String, u16) {
    let max_text_width = width.saturating_sub(1) as usize;
    if max_text_width == 0 {
        return (String::new(), 0);
    }
    let cursor = cursor.min(value.len());
    let line_start = value[..cursor]
        .rfind('\n')
        .map(|index| index + 1)
        .unwrap_or(0);
    let line_end = value[cursor..]
        .find('\n')
        .map(|offset| cursor + offset)
        .unwrap_or(value.len());
    let line = &value[line_start..line_end];
    let cursor_in_line = cursor.saturating_sub(line_start);

    let right_context_width = line[cursor_in_line..]
        .graphemes(true)
        .next()
        .map(UnicodeWidthStr::width)
        .unwrap_or(0)
        .min(max_text_width);
    let before_budget = max_text_width.saturating_sub(right_context_width);
    let mut start = cursor_in_line;
    let mut before_width = 0usize;
    for (index, grapheme) in line[..cursor_in_line].grapheme_indices(true).rev() {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if before_width.saturating_add(grapheme_width) > before_budget {
            break;
        }
        start = index;
        before_width = before_width.saturating_add(grapheme_width);
    }

    let mut visible = String::new();
    let mut visible_width = 0usize;
    for grapheme in line[start..].graphemes(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if visible_width.saturating_add(grapheme_width) > max_text_width {
            break;
        }
        visible.push_str(grapheme);
        visible_width = visible_width.saturating_add(grapheme_width);
    }
    (visible, before_width as u16)
}

#[derive(Clone, Copy)]
pub(crate) enum Truncation {
    Right,
    Middle,
}

pub(crate) fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}

pub(crate) fn format_elapsed(duration: std::time::Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m {}s", seconds / 60, seconds % 60)
    }
}

pub(crate) fn truncate_display(value: &str, max_width: usize, mode: Truncation) -> String {
    if UnicodeWidthStr::width(value) <= max_width {
        return value.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".into();
    }
    let take_width = |text: &str, budget: usize, reverse: bool| {
        let graphemes = UnicodeSegmentation::graphemes(text, true).collect::<Vec<_>>();
        let iterator: Box<dyn Iterator<Item = &&str>> = if reverse {
            Box::new(graphemes.iter().rev())
        } else {
            Box::new(graphemes.iter())
        };
        let mut width = 0;
        let mut retained = Vec::new();
        for grapheme in iterator {
            let grapheme_width = UnicodeWidthStr::width(*grapheme);
            if width + grapheme_width > budget {
                break;
            }
            retained.push(*grapheme);
            width += grapheme_width;
        }
        if reverse {
            retained.reverse();
        }
        retained.concat()
    };
    match mode {
        Truncation::Right => format!("{}…", take_width(value, max_width - 1, false)),
        Truncation::Middle => {
            let left = (max_width - 1) / 2;
            let right = max_width - 1 - left;
            format!(
                "{}…{}",
                take_width(value, left, false),
                take_width(value, right, true)
            )
        }
    }
}

pub(crate) fn abbreviated_path(path: &std::path::Path, home: Option<&std::path::Path>) -> String {
    if let Some(home) = home
        && let Ok(relative) = path.strip_prefix(home)
    {
        return if relative.as_os_str().is_empty() {
            "~".into()
        } else {
            format!("~/{}", relative.display())
        };
    }
    path.display().to_string()
}

fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}m", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

pub(crate) fn format_tokens_compact(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{}m", tokens / 1_000_000)
    } else if tokens >= 1_000 {
        format!("{}k", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

pub(crate) fn context_usage(used: u64, limit: Option<u64>, estimated: bool) -> String {
    let estimate = if estimated { "~" } else { "" };
    match limit.filter(|limit| *limit > 0) {
        Some(limit) => format!(
            "ctx {estimate}{}/{} · {}%",
            format_tokens(used),
            format_tokens(limit),
            used.saturating_mul(100) / limit
        ),
        None => format!("ctx {estimate}{}/? · —%", format_tokens(used)),
    }
}

pub(crate) fn context_meter(
    used: u64,
    limit: Option<u64>,
    estimated: bool,
    cells: usize,
) -> Option<String> {
    let limit = limit.filter(|limit| *limit > 0)?;
    let percent = used.saturating_mul(100) / limit;
    let filled = (percent.min(100) as usize)
        .saturating_mul(cells)
        .saturating_add(99)
        / 100;
    Some(format!(
        "ctx [{}{}] {}{}%",
        "█".repeat(filled),
        "░".repeat(cells.saturating_sub(filled)),
        if estimated { "~" } else { "" },
        percent
    ))
}

pub(crate) fn safe_text(text: &str) -> String {
    let mut output = String::new();
    let mut column = 0usize;
    for character in text.chars().filter(|c| *c == '\t' || !c.is_control()) {
        if character == '\t' {
            let spaces = 4 - column % 4;
            output.push_str(&" ".repeat(spaces));
            column += spaces;
        } else {
            output.push(character);
            column += 1;
        }
    }
    output
}

pub(crate) fn safe_lines(text: &str) -> Vec<String> {
    let lines: Vec<_> = text.lines().map(safe_text).collect();
    if lines.is_empty() {
        vec![String::new()]
    } else {
        lines
    }
}

pub(crate) fn bounded_detail(text: &str) -> String {
    const MAX_DETAIL_CHARS: usize = 16 * 1024;
    let mut detail = text.lines().map(safe_text).collect::<Vec<_>>().join("\n");
    if detail.chars().count() > MAX_DETAIL_CHARS {
        detail = detail.chars().take(MAX_DETAIL_CHARS).collect();
        detail.push_str("\n… output truncated");
    }
    detail
}
#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Flatten a styled line to plain text. Shared with main.rs's tests.
    pub(crate) fn rendered_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn format_cost_buckets() {
        assert_eq!(format_cost(0.0), "$0.00");
        assert_eq!(format_cost(0.0001), "$<0.001");
        assert_eq!(format_cost(0.004375), "$0.004");
        assert_eq!(format_cost(0.42), "$0.420");
        assert_eq!(format_cost(1.234), "$1.23");
    }

    #[test]
    fn styled_wrap_prefers_words_and_never_adds_blank_rows() {
        let code = crate::markdown::render("```\n    hello world\n```", usize::MAX).remove(0);
        let original_code = rendered_text(&code);
        let wrapped = wrap_markdown_line(code, 5);

        assert!(wrapped.iter().all(|line| !rendered_text(line).is_empty()));
        assert_eq!(
            wrapped.iter().map(rendered_text).collect::<String>(),
            original_code
        );
        assert!(wrapped.iter().flat_map(|line| &line.spans).all(|span| {
            span.style.fg == Some(theme::CODE) || span.style.fg == Some(theme::PRIMARY)
        }));

        let inline = crate::markdown::render("`│ alpha beta`", usize::MAX).remove(0);
        assert_eq!(
            wrap_markdown_line(inline, 8)
                .iter()
                .map(rendered_text)
                .collect::<Vec<_>>(),
            ["│ alpha", "beta"]
        );

        let wide = wrap_styled_line(Line::raw("界界"), 2);
        assert_eq!(wide.len(), 2);
        assert!(wide.iter().all(|line| line.width() == 2));
        assert_eq!(rendered_text(&wrap_styled_line(Line::raw("界"), 1)[0]), "…");

        let words = wrap_styled_line(Line::raw("alpha beta gamma"), 10)
            .iter()
            .map(rendered_text)
            .collect::<Vec<_>>();
        assert_eq!(words, ["alpha beta", "gamma"]);

        let long_word = wrap_styled_line(Line::raw("abcdefgh"), 5)
            .iter()
            .map(rendered_text)
            .collect::<Vec<_>>();
        assert_eq!(long_word, ["abcde", "fgh"]);
    }
}
