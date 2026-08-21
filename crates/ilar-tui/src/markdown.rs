use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::theme;

#[derive(Clone, Copy)]
enum ColumnAlignment {
    Left,
    Center,
    Right,
}

struct MarkdownTable {
    headers: Vec<String>,
    alignments: Vec<ColumnAlignment>,
    rows: Vec<Vec<String>>,
}

/// Code speaks its own language. Borrowing the status colours meant a
/// string literal and a passing tool call were the same green, so a fence
/// read as more status rather than as code.
fn highlight_color(class: crate::highlight::Class) -> ratatui::style::Color {
    use crate::highlight::Class;
    match class {
        Class::Keyword => theme::SYN_KEYWORD,
        Class::String => theme::SYN_STRING,
        Class::Comment => theme::SYN_COMMENT,
        Class::Number => theme::SYN_NUMBER,
        Class::Plain => theme::PRIMARY,
    }
}

/// Render the Markdown subset used in agent responses into terminal-native
/// lines while constraining tables to `width` cells. Incomplete delimiters
/// remain literal, which keeps streaming output readable.
pub fn render(source: &str, width: usize) -> Vec<Line<'static>> {
    let source = sanitize(source);
    let source_lines = source.lines().collect::<Vec<_>>();
    let mut lines = Vec::new();
    let mut code_fence: Option<(char, usize)> = None;
    let mut code_language: Option<crate::highlight::Language> = None;
    let mut code_state = crate::highlight::BlockState::default();
    let mut pending_separator = false;
    let mut index = 0;

    while index < source_lines.len() {
        let raw = source_lines[index];
        let current_index = index;
        index += 1;
        let trimmed = raw.trim_start();
        if let Some((fence, length, suffix)) = fence(trimmed) {
            if let Some((open_fence, open_length)) = code_fence {
                if fence == open_fence && length >= open_length && suffix.trim().is_empty() {
                    code_fence = None;
                    code_language = None;
                    continue;
                }
            } else {
                code_fence = Some((fence, length));
                code_language = crate::highlight::language_for(suffix.trim());
                code_state = crate::highlight::BlockState::default();
                flush_separator(&mut lines, &mut pending_separator);
                let language = suffix.trim();
                if !language.is_empty() {
                    lines.push(Line::from(Span::styled(
                        format!("  {language}"),
                        Style::default().fg(theme::MUTED),
                    )));
                }
                continue;
            }
        }

        if code_fence.is_some() {
            let mut spans = vec![Span::styled("│ ", Style::default().fg(theme::CODE))];
            match code_language {
                Some(language) => {
                    let expanded = expand_tabs(raw);
                    for (class, text) in
                        crate::highlight::highlight_line(language, &expanded, &mut code_state)
                    {
                        spans.push(Span::styled(
                            text,
                            Style::default().fg(highlight_color(class)),
                        ));
                    }
                }
                None => spans.push(Span::styled(
                    expand_tabs(raw),
                    Style::default().fg(theme::PRIMARY),
                )),
            }
            lines.push(Line::from(spans));
            continue;
        }

        if trimmed.is_empty() {
            pending_separator = true;
            continue;
        }

        if let Some((table, consumed)) = parse_table(&source_lines, current_index) {
            flush_separator(&mut lines, &mut pending_separator);
            lines.extend(render_table(table, width));
            index = current_index + consumed;
            continue;
        }

        flush_separator(&mut lines, &mut pending_separator);

        if let Some((level, text)) = heading(trimmed) {
            let (prefix, color) = match level {
                1 => ("▌ ", theme::MARKUP),
                2 => ("◆ ", theme::MARKUP),
                _ => ("› ", theme::MARKUP),
            };
            let style = Style::default().fg(color).add_modifier(Modifier::BOLD);
            let mut spans = vec![Span::styled(prefix.to_string(), style)];
            spans.extend(render_inline(text, style));
            lines.push(Line::from(spans));
            continue;
        }

        if let Some(text) = trimmed.strip_prefix("> ") {
            let style = Style::default()
                .fg(theme::MUTED)
                .add_modifier(Modifier::ITALIC);
            let mut spans = vec![Span::styled("│ ", style)];
            spans.extend(render_inline(text, style));
            lines.push(Line::from(spans));
            continue;
        }

        if is_rule(trimmed) {
            lines.push(Line::from(Span::styled(
                "────────────────────────",
                Style::default().fg(theme::MUTED),
            )));
            continue;
        }

        if let Some((indent, marker, text)) = list_item(raw) {
            let mut spans = vec![Span::styled(
                format!("{}{} ", "  ".repeat(indent), marker),
                Style::default().fg(theme::MARKUP),
            )];
            spans.extend(render_inline(text, Style::default()));
            lines.push(Line::from(spans));
            continue;
        }

        lines.push(Line::from(render_inline(
            &expand_tabs(raw),
            Style::default(),
        )));
    }

    lines
}

fn parse_table(lines: &[&str], start: usize) -> Option<(MarkdownTable, usize)> {
    let headers = split_table_row(lines.get(start)?)?;
    let delimiter_cells = split_table_row(lines.get(start + 1)?)?;
    if headers.len() != delimiter_cells.len() {
        return None;
    }
    let alignments = delimiter_cells
        .iter()
        .map(|cell| delimiter_alignment(cell))
        .collect::<Option<Vec<_>>>()?;

    let column_count = headers.len();
    let mut rows = Vec::new();
    let mut consumed = 2;
    while let Some(raw) = lines.get(start + consumed) {
        if raw.trim().is_empty() {
            break;
        }
        if starts_markdown_block(raw) {
            break;
        }
        let Some(mut row) = split_table_row(raw) else {
            break;
        };
        row.resize(column_count, String::new());
        row.truncate(column_count);
        rows.push(row);
        consumed += 1;
    }

    Some((
        MarkdownTable {
            headers,
            alignments,
            rows,
        },
        consumed,
    ))
}

fn split_table_row(line: &str) -> Option<Vec<String>> {
    let line = line.trim();
    let characters = line.chars().collect::<Vec<_>>();
    let mut cells = vec![String::new()];
    let mut separators = 0;
    let mut code_fence = None;
    let mut index = 0;

    while index < characters.len() {
        let character = characters[index];
        if character == '\\' {
            let run = characters[index..]
                .iter()
                .take_while(|character| **character == '\\')
                .count();
            if characters.get(index + run) == Some(&'|') {
                cells.last_mut()?.extend(std::iter::repeat_n('\\', run / 2));
                if run % 2 == 1 {
                    cells.last_mut()?.push('|');
                    index += run + 1;
                } else {
                    index += run;
                }
            } else {
                cells.last_mut()?.extend(std::iter::repeat_n('\\', run));
                index += run;
            }
            continue;
        }
        if character == '`' {
            let run = characters[index..]
                .iter()
                .take_while(|character| **character == '`')
                .count();
            if code_fence == Some(run) {
                code_fence = None;
            } else if code_fence.is_none() {
                code_fence = Some(run);
            }
            cells.last_mut()?.extend(std::iter::repeat_n('`', run));
            index += run;
            continue;
        }
        if character == '|' && code_fence.is_none() {
            cells.push(String::new());
            separators += 1;
        } else {
            cells.last_mut()?.push(character);
        }
        index += 1;
    }

    if separators == 0 {
        return None;
    }
    if cells.first().is_some_and(|cell| cell.trim().is_empty()) {
        cells.remove(0);
    }
    if cells.last().is_some_and(|cell| cell.trim().is_empty()) {
        cells.pop();
    }
    if cells.is_empty() {
        return None;
    }
    Some(
        cells
            .into_iter()
            .map(|cell| expand_tabs(cell.trim()))
            .collect(),
    )
}

fn starts_markdown_block(line: &str) -> bool {
    let trimmed = line.trim_start();
    fence(trimmed).is_some()
        || starts_atx_heading(trimmed)
        || trimmed.starts_with('>')
        || starts_list_marker(trimmed)
        || is_rule(trimmed)
}

fn starts_atx_heading(line: &str) -> bool {
    let hashes = line
        .chars()
        .take_while(|character| *character == '#')
        .count();
    (1..=6).contains(&hashes)
        && line[hashes..]
            .chars()
            .next()
            .is_none_or(char::is_whitespace)
}

fn starts_list_marker(line: &str) -> bool {
    if let Some(rest) = line.strip_prefix(['-', '*', '+']) {
        return rest.chars().next().is_none_or(char::is_whitespace);
    }
    let digits = line
        .bytes()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digits == 0 {
        return false;
    }
    line[digits..]
        .strip_prefix(['.', ')'])
        .is_some_and(|rest| rest.chars().next().is_none_or(char::is_whitespace))
}

fn delimiter_alignment(cell: &str) -> Option<ColumnAlignment> {
    let cell = cell.trim();
    let left = cell.starts_with(':');
    let right = cell.ends_with(':');
    let dashes = cell.trim_start_matches(':').trim_end_matches(':');
    if dashes.is_empty() || !dashes.chars().all(|character| character == '-') {
        return None;
    }
    Some(match (left, right) {
        (true, true) => ColumnAlignment::Center,
        (false, true) => ColumnAlignment::Right,
        _ => ColumnAlignment::Left,
    })
}

fn render_table(table: MarkdownTable, width: usize) -> Vec<Line<'static>> {
    let separator_width = 3usize.saturating_mul(table.headers.len().saturating_sub(1));
    let available = width.saturating_sub(separator_width);
    if available >= 10usize.saturating_mul(table.headers.len()) {
        render_grid_table(table, available)
    } else {
        render_stacked_table(table, width)
    }
}

fn render_grid_table(table: MarkdownTable, available: usize) -> Vec<Line<'static>> {
    let header_style = Style::default().add_modifier(Modifier::BOLD);
    let mut widths = (0..table.headers.len())
        .map(|column| {
            std::iter::once(&table.headers[column])
                .chain(table.rows.iter().map(|row| &row[column]))
                .map(|cell| inline_width(cell, Style::default()))
                .max()
                .unwrap_or(1)
                .max(3)
                .min(available)
        })
        .collect::<Vec<_>>();
    while widths.iter().sum::<usize>() > available {
        let Some(column) = widths
            .iter()
            .enumerate()
            .filter(|(_, width)| **width > 3)
            .max_by_key(|(_, width)| **width)
            .map(|(column, _)| column)
        else {
            break;
        };
        widths[column] -= 1;
    }

    let mut lines = render_grid_row(&table.headers, &widths, &table.alignments, header_style);
    lines.push(table_rule(&widths));
    for row in table.rows {
        lines.extend(render_grid_row(
            &row,
            &widths,
            &table.alignments,
            Style::default(),
        ));
    }
    lines
}

fn render_grid_row(
    cells: &[String],
    widths: &[usize],
    alignments: &[ColumnAlignment],
    base: Style,
) -> Vec<Line<'static>> {
    let wrapped = cells
        .iter()
        .zip(widths)
        .map(|(cell, width)| bounded_wrap(Line::from(render_inline(cell, base)), *width))
        .collect::<Vec<_>>();
    let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
    let separator_style = Style::default().fg(theme::MUTED);
    let mut output = Vec::with_capacity(height);

    for row in 0..height {
        let mut spans = Vec::new();
        for column in 0..cells.len() {
            if column > 0 {
                spans.push(Span::styled(" │ ", separator_style));
            }
            let line = wrapped[column].get(row).cloned().unwrap_or_default();
            spans.extend(pad_cell(line, widths[column], alignments[column]));
        }
        output.push(Line::from(spans));
    }
    output
}

fn pad_cell(
    mut line: Line<'static>,
    width: usize,
    alignment: ColumnAlignment,
) -> Vec<Span<'static>> {
    let padding = width.saturating_sub(line.width());
    let (left, right) = match alignment {
        ColumnAlignment::Left => (0, padding),
        ColumnAlignment::Center => (padding / 2, padding - padding / 2),
        ColumnAlignment::Right => (padding, 0),
    };
    let mut spans = Vec::new();
    if left > 0 {
        spans.push(Span::raw(" ".repeat(left)));
    }
    spans.append(&mut line.spans);
    if right > 0 {
        spans.push(Span::raw(" ".repeat(right)));
    }
    spans
}

fn table_rule(widths: &[usize]) -> Line<'static> {
    let style = Style::default().fg(theme::MUTED);
    let mut spans = Vec::new();
    for (column, width) in widths.iter().enumerate() {
        if column > 0 {
            spans.push(Span::styled("─┼─", style));
        }
        spans.push(Span::styled("─".repeat(*width), style));
    }
    Line::from(spans)
}

fn render_stacked_table(table: MarkdownTable, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return vec![Line::default()];
    }
    let label_style = Style::default()
        .fg(theme::MUTED)
        .add_modifier(Modifier::BOLD);
    let labels = table
        .headers
        .iter()
        .map(|header| {
            let mut label = render_inline(header, label_style);
            label.push(Span::styled(":", label_style));
            Line::from(label)
        })
        .collect::<Vec<_>>();
    let label_width = labels.iter().map(Line::width).max().unwrap_or(0);
    let inline_values = label_width.saturating_add(14) <= width;
    let mut output = Vec::new();

    for (row_index, row) in table.rows.iter().enumerate() {
        if row_index > 0 {
            output.push(Line::default());
        }
        for (column, cell) in row.iter().enumerate() {
            let value = Line::from(render_inline(cell, Style::default()));
            if inline_values {
                let value_width = width - label_width - 1;
                let values = bounded_wrap(value, value_width);
                for (line_index, mut value) in values.into_iter().enumerate() {
                    let mut spans = if line_index == 0 {
                        pad_cell(labels[column].clone(), label_width, ColumnAlignment::Left)
                    } else {
                        vec![Span::raw(" ".repeat(label_width))]
                    };
                    spans.push(Span::raw(" "));
                    spans.append(&mut value.spans);
                    output.push(Line::from(spans));
                }
            } else {
                output.extend(bounded_wrap(labels[column].clone(), width));
                output.extend(bounded_wrap(value, width));
            }
        }
    }
    if table.rows.is_empty() {
        for label in labels {
            output.extend(bounded_wrap(label, width));
        }
    }
    output
}

fn inline_width(cell: &str, base: Style) -> usize {
    render_inline(cell, base)
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

fn bounded_wrap(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    crate::text::wrap_styled_line(line, width)
        .into_iter()
        .map(|line| {
            if line.width() <= width {
                line
            } else if width == 0 {
                Line::default()
            } else {
                let style = line
                    .spans
                    .first()
                    .map(|span| span.style)
                    .unwrap_or_default();
                Line::from(Span::styled("…", style))
            }
        })
        .collect()
}

fn flush_separator(lines: &mut Vec<Line<'static>>, pending: &mut bool) {
    if *pending && !lines.is_empty() {
        lines.push(Line::default());
    }
    *pending = false;
}

fn sanitize(source: &str) -> String {
    source
        .chars()
        .filter(|c| *c == '\n' || *c == '\t' || !c.is_control())
        .collect()
}

fn expand_tabs(line: &str) -> String {
    let mut output = String::new();
    let mut column = 0usize;
    for character in line.chars() {
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

fn fence(line: &str) -> Option<(char, usize, &str)> {
    let fence = line.chars().next()?;
    if !matches!(fence, '`' | '~') {
        return None;
    }
    let length = line
        .chars()
        .take_while(|character| *character == fence)
        .count();
    if length < 3 {
        return None;
    }
    Some((fence, length, line.get(length..)?))
}

fn heading(line: &str) -> Option<(usize, &str)> {
    let level = line.chars().take_while(|c| *c == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    Some((level, line.get(level..)?.strip_prefix(' ')?))
}

fn list_item(line: &str) -> Option<(usize, String, &str)> {
    let spaces = line.len() - line.trim_start_matches(' ').len();
    let line = &line[spaces..];
    for prefix in ["- ", "* ", "+ "] {
        if let Some(text) = line.strip_prefix(prefix) {
            return Some((spaces / 2, "•".into(), text));
        }
    }
    let digits = line
        .bytes()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digits > 0 && line.get(digits..)?.starts_with(". ") {
        return Some((spaces / 2, line[..=digits].to_string(), &line[digits + 2..]));
    }
    None
}

fn is_rule(line: &str) -> bool {
    let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    compact.len() >= 3
        && compact
            .chars()
            .all(|c| c == compact.chars().next().unwrap())
        && matches!(compact.chars().next(), Some('-' | '*' | '_'))
}

fn render_inline(text: &str, base: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = text;

    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("**") {
            if let Some(end) = after.find("**") {
                spans.push(Span::styled(
                    after[..end].to_string(),
                    base.add_modifier(Modifier::BOLD),
                ));
                rest = &after[end + 2..];
            } else {
                spans.push(Span::styled("**", base));
                rest = after;
            }
            continue;
        }
        if let Some(after) = rest.strip_prefix('*')
            && let Some(end) = after.find('*')
        {
            spans.push(Span::styled(
                after[..end].to_string(),
                base.add_modifier(Modifier::ITALIC),
            ));
            rest = &after[end + 1..];
            continue;
        }
        if rest.starts_with('`') {
            let opening = rest.bytes().take_while(|byte| *byte == b'`').count();
            if let Some(end) = matching_backticks(rest, opening) {
                let content = &rest[opening..end];
                // A tint, not reverse video: inline code is the most
                // common inline element there is, and reversing it put a
                // white slab in the middle of every other sentence.
                spans.push(Span::styled(
                    content.to_string(),
                    base.fg(theme::CODE).bg(theme::CODE_BG),
                ));
                rest = &rest[end + opening..];
            } else {
                spans.push(Span::styled("`".repeat(opening), base));
                rest = &rest[opening..];
            }
            continue;
        }
        if let Some(after) = rest.strip_prefix('[')
            && let Some(label_end) = after.find("](")
            && let Some(url_end) = after[label_end + 2..].find(')')
        {
            let url_start = label_end + 2;
            let url = &after[url_start..url_start + url_end];
            spans.push(Span::styled(
                after[..label_end].to_string(),
                base.add_modifier(Modifier::UNDERLINED),
            ));
            spans.push(Span::styled(format!(" <{url}>"), base.fg(theme::MUTED)));
            rest = &after[url_start + url_end + 1..];
            continue;
        }

        let next = rest
            .char_indices()
            .skip(1)
            .find(|(_, c)| matches!(c, '*' | '`' | '['))
            .map(|(index, _)| index)
            .unwrap_or(rest.len());
        spans.push(Span::styled(rest[..next].to_string(), base));
        rest = &rest[next..];
    }
    spans
}

fn matching_backticks(text: &str, length: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut index = length;
    while index < bytes.len() {
        if bytes[index] != b'`' {
            index += 1;
            continue;
        }
        let run = bytes[index..]
            .iter()
            .take_while(|byte| **byte == b'`')
            .count();
        if run == length {
            return Some(index);
        }
        index += run;
    }
    None
}

#[cfg(test)]
mod tests {
    use ratatui::style::Modifier;
    use unicode_width::UnicodeWidthStr;

    use super::render as render_with_width;
    use crate::theme;

    fn render(source: &str) -> Vec<ratatui::text::Line<'static>> {
        render_with_width(source, usize::MAX)
    }

    fn text(line: &ratatui::text::Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn renders_blocks_as_separate_terminal_lines() {
        let lines =
            render("## Overview\n\nFirst paragraph.\nSecond line.\n\n- one\n- two\n> quoted");
        let rendered: Vec<String> = lines.iter().map(text).collect();

        assert!(rendered.contains(&"◆ Overview".to_string()));
        assert!(rendered.contains(&"First paragraph.".to_string()));
        assert!(rendered.contains(&"Second line.".to_string()));
        assert!(rendered.contains(&"• one".to_string()));
        assert!(rendered.contains(&"│ quoted".to_string()));
        assert!(lines.iter().all(|line| !text(line).contains('\n')));
        let heading = lines
            .iter()
            .find(|line| text(line) == "◆ Overview")
            .unwrap();
        assert_eq!(heading.spans[0].style.fg, Some(theme::MARKUP));
        assert!(heading.spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn paragraph_separators_collapse_to_one_interior_blank_row() {
        let lines = render("\n\nFirst paragraph.\n\n\nSecond paragraph.\n\n");
        let rendered: Vec<String> = lines.iter().map(text).collect();

        assert_eq!(rendered, ["First paragraph.", "", "Second paragraph."]);
    }

    #[test]
    fn styles_inline_markdown() {
        let lines =
            render("Use **bold**, *care*, `cargo test`, and [Ratatui](https://ratatui.rs).");
        let spans = &lines[0].spans;

        assert!(spans.iter().any(|span| {
            span.content == "bold" && span.style.add_modifier.contains(Modifier::BOLD)
        }));
        assert!(spans.iter().any(|span| {
            span.content == "care" && span.style.add_modifier.contains(Modifier::ITALIC)
        }));
        assert!(spans.iter().any(|span| {
            span.content == "cargo test"
                && span.style.fg == Some(theme::CODE)
                && span.style.bg == Some(theme::CODE_BG)
                && !span.style.add_modifier.contains(Modifier::REVERSED)
        }));
        assert!(spans.iter().any(|span| {
            span.content == "Ratatui" && span.style.add_modifier.contains(Modifier::UNDERLINED)
        }));
        assert!(text(&lines[0]).contains("<https://ratatui.rs>"));
    }

    #[test]
    fn fenced_code_preserves_lines_and_has_a_gutter() {
        let lines = render("```rust\nfn main() {\n\n    println!(\"hi\");\n}\n```");
        let rendered: Vec<String> = lines.iter().map(text).collect();

        assert_eq!(rendered[0], "  rust");
        assert_eq!(rendered[1], "│ fn main() {");
        assert_eq!(rendered[2], "│ ");
        assert_eq!(rendered[3], "│     println!(\"hi\");");
        assert_eq!(lines[1].spans[0].style.fg, Some(theme::CODE));
        // `fn` is highlighted as a keyword; the rest of the signature is plain.
        assert_eq!(lines[1].spans[1].style.fg, Some(theme::SYN_KEYWORD));
        assert_eq!(lines[1].spans[1].content, "fn");
        assert_eq!(lines[1].spans[2].style.fg, Some(theme::PRIMARY));
    }

    #[test]
    fn fenced_code_highlights_by_language_and_stays_plain_otherwise() {
        let lines = render("```rust\nlet s = \"text\"; // note\n```");
        let code = &lines[1];
        let span_for = |needle: &str| {
            code.spans
                .iter()
                .find(|span| span.content.contains(needle))
                .unwrap_or_else(|| panic!("missing {needle:?}: {code:?}"))
                .style
                .fg
        };
        // Syntax classes resolve per theme and never borrow a status
        // colour's slot, so a string literal is not the success green.
        assert_eq!(span_for("let"), Some(theme::SYN_KEYWORD));
        assert_eq!(span_for("\"text\""), Some(theme::SYN_STRING));
        assert_eq!(span_for("// note"), Some(theme::SYN_COMMENT));
        assert_eq!(text(code), "│ let s = \"text\"; // note");

        // Unknown language: single plain span as before.
        let plain = render("```brainfuck\n+[----->+++<]\n```");
        assert_eq!(plain[1].spans.len(), 2);
        assert_eq!(plain[1].spans[1].style.fg, Some(theme::PRIMARY));

        // Streaming: an unclosed fence still renders highlighted lines.
        let streaming = render("```rust\nlet x = 1;");
        assert_eq!(text(&streaming[1]), "│ let x = 1;");
        assert_eq!(streaming[1].spans[1].style.fg, Some(theme::SYN_KEYWORD));
    }

    #[test]
    fn incomplete_bold_delimiters_remain_literal_while_streaming() {
        for input in ["**", "**streaming", "**bold*"] {
            assert_eq!(text(&render(input)[0]), input);
        }
    }

    #[test]
    fn longer_and_tilde_fences_close_only_with_a_matching_fence() {
        let lines = render("````rust\n``` is code\n````\n\n~~~text\nhello\n~~~");
        let rendered: Vec<String> = lines.iter().map(text).collect();

        assert!(rendered.contains(&"│ ``` is code".to_string()));
        assert!(rendered.contains(&"│ hello".to_string()));
        assert!(!rendered.iter().any(|line| line.contains("````")));
    }

    #[test]
    fn tabs_expand_to_stable_four_column_stops() {
        let lines = render("```\n\tlet x = 1;\n  \treturn x;\n```");
        assert_eq!(text(&lines[0]), "│     let x = 1;");
        assert_eq!(text(&lines[1]), "│     return x;");
    }

    #[test]
    fn complete_tables_render_as_styled_ruled_rows() {
        let lines = render_with_width(
            "| Phase | Estimate |\n| :--- | ---: |\n| **Signed-device testing** | 1–2 weeks |\n| Escaped \\| pipe | `x|y` |",
            80,
        );
        let rendered: Vec<String> = lines.iter().map(text).collect();

        assert_eq!(rendered.len(), 4);
        assert!(rendered[0].contains("Phase"));
        assert!(rendered[0].contains('│'));
        assert!(rendered[1].contains('┼'));
        assert!(!rendered.iter().any(|line| line.contains("---")));
        assert!(rendered[2].ends_with("1–2 weeks"));
        assert!(rendered[3].contains("Escaped | pipe"));
        assert!(rendered[3].contains("x|y"));
        assert!(lines[0].spans.iter().any(|span| {
            span.content.contains("Phase") && span.style.add_modifier.contains(Modifier::BOLD)
        }));
        assert!(lines[2].spans.iter().any(|span| {
            span.content.contains("Signed-device testing")
                && span.style.add_modifier.contains(Modifier::BOLD)
        }));
        assert!(lines[3].spans.iter().any(|span| {
            span.content == "x|y"
                && span.style.fg == Some(theme::CODE)
                && span.style.bg == Some(theme::CODE_BG)
                && !span.style.add_modifier.contains(Modifier::REVERSED)
        }));
    }

    #[test]
    fn narrow_tables_render_as_bounded_labeled_records() {
        let lines = render_with_width(
            "| Phase | Estimate |\n| --- | ---: |\n| Signed-device testing across iOS and Android | 1–2 weeks |\n| Unicode 界界界 validation | Complete |",
            22,
        );
        let rendered: Vec<String> = lines.iter().map(text).collect();

        assert!(rendered.iter().any(|line| line.contains("Phase:")));
        assert!(rendered.iter().any(|line| line.contains("Estimate:")));
        assert!(rendered.iter().any(|line| line.contains("Signed-device")));
        assert!(rendered.iter().any(|line| line.contains("界界界")));
        assert!(!rendered.iter().any(|line| line.contains("---")));
        assert!(
            rendered
                .iter()
                .all(|line| UnicodeWidthStr::width(line.as_str()) <= 22)
        );
    }

    #[test]
    fn incomplete_streaming_table_syntax_stays_literal() {
        let lines = render_with_width("| Phase | Estimate |\n| --- |", 80);
        let rendered: Vec<String> = lines.iter().map(text).collect();

        assert_eq!(rendered, ["| Phase | Estimate |", "| --- |"]);
    }

    #[test]
    fn renders_single_column_tables_with_outer_pipes() {
        let lines = render_with_width("| Status\n| -\n| Ready", 40);
        let rendered: Vec<String> = lines.iter().map(text).collect();

        assert_eq!(rendered, ["Status", "──────", "Ready "]);
    }

    #[test]
    fn table_body_stops_before_another_markdown_block() {
        let lines = render_with_width("| A | B |\n| - | - |\n| 1 | 2 |\n## Next | section", 40);
        let rendered: Vec<String> = lines.iter().map(text).collect();

        assert_eq!(rendered.last().unwrap(), "◆ Next | section");
        assert_eq!(rendered.len(), 4);
    }

    #[test]
    fn table_body_stops_before_compact_or_tabbed_block_markers() {
        for marker in [">quote | value", "#\tHeading | value", "-\titem | value"] {
            let source = format!("| A | B |\n| - | - |\n{marker}");
            let rendered = render_with_width(&source, 40)
                .iter()
                .map(text)
                .collect::<Vec<_>>();

            assert!(rendered.last().unwrap().contains(" | "), "{rendered:?}");
            assert!(!rendered.last().unwrap().contains('│'), "{rendered:?}");
        }
    }

    #[test]
    fn multiple_backticks_protect_and_style_pipes_in_code_spans() {
        let lines = render_with_width("| Code | State |\n| - | - |\n| ``a`b|c`` | ready |", 40);

        assert!(text(&lines[2]).contains("a`b|c"));
        assert!(lines[2].spans.iter().any(|span| {
            span.content == "a`b|c"
                && span.style.fg == Some(theme::CODE)
                && span.style.bg == Some(theme::CODE_BG)
                && !span.style.add_modifier.contains(Modifier::REVERSED)
        }));
    }

    #[test]
    fn even_backslashes_leave_a_pipe_as_a_column_separator() {
        let lines = render_with_width("| One | Two | Three |\n| - | - | - |\n| a\\\\| b | c |", 60);
        let row = text(&lines[2]);
        let cells = row.split('│').map(str::trim).collect::<Vec<_>>();

        assert_eq!(cells, ["a\\", "b", "c"]);
    }

    #[test]
    fn tabs_in_table_cells_expand_to_visible_spacing() {
        let lines = render_with_width("| Value |\n| - |\n| a\tb |", 40);

        assert!(text(&lines[2]).starts_with("a   b"));
    }

    #[test]
    fn partial_body_rows_and_tiny_widths_remain_bounded() {
        let complete = "| A | B |\n| - | - |\n| partial | row |";
        for width in 0..=2 {
            let lines = render_with_width(complete, width);
            assert!(lines.iter().all(|line| line.width() <= width));
        }

        let partial = "| A | B |\n| - | - |\n| partial";
        let rendered = render_with_width(partial, 40)
            .iter()
            .map(text)
            .collect::<Vec<_>>();
        assert!(rendered.iter().any(|line| line.contains("partial")));
    }
}
