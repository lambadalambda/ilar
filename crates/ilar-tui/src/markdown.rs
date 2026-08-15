use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Render the Markdown subset used in agent responses into terminal-native
/// lines. Incomplete delimiters remain literal, which keeps streaming output
/// readable while a response is still arriving.
pub fn render(source: &str) -> Vec<Line<'static>> {
    let source = sanitize(source);
    let mut lines = Vec::new();
    let mut code_fence: Option<(char, usize)> = None;

    for raw in source.lines() {
        let trimmed = raw.trim_start();
        if let Some((fence, length, suffix)) = fence(trimmed) {
            if let Some((open_fence, open_length)) = code_fence {
                if fence == open_fence && length >= open_length && suffix.trim().is_empty() {
                    code_fence = None;
                    continue;
                }
            } else {
                code_fence = Some((fence, length));
                let language = suffix.trim();
                if !language.is_empty() {
                    lines.push(Line::from(Span::styled(
                        format!("  {language}"),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
                continue;
            }
        }

        if code_fence.is_some() {
            lines.push(Line::from(Span::styled(
                format!("│ {}", expand_tabs(raw)),
                Style::default().fg(Color::Cyan),
            )));
            continue;
        }

        if trimmed.is_empty() {
            lines.push(Line::default());
            continue;
        }

        if let Some((level, text)) = heading(trimmed) {
            let (prefix, color) = match level {
                1 => ("▌ ", Color::Magenta),
                2 => ("◆ ", Color::Magenta),
                _ => ("› ", Color::Blue),
            };
            let style = Style::default().fg(color).add_modifier(Modifier::BOLD);
            let mut spans = vec![Span::styled(prefix.to_string(), style)];
            spans.extend(render_inline(text, style));
            lines.push(Line::from(spans));
            continue;
        }

        if let Some(text) = trimmed.strip_prefix("> ") {
            let style = Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC);
            let mut spans = vec![Span::styled("│ ", style)];
            spans.extend(render_inline(text, style));
            lines.push(Line::from(spans));
            continue;
        }

        if is_rule(trimmed) {
            lines.push(Line::from(Span::styled(
                "────────────────────────",
                Style::default().fg(Color::DarkGray),
            )));
            continue;
        }

        if let Some((indent, marker, text)) = list_item(raw) {
            let mut spans = vec![Span::styled(
                format!("{}{} ", "  ".repeat(indent), marker),
                Style::default().fg(Color::Blue),
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

    if lines.is_empty() {
        lines.push(Line::default());
    }
    lines
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
        if let Some(after) = rest.strip_prefix('`')
            && let Some(end) = after.find('`')
        {
            spans.push(Span::styled(
                after[..end].to_string(),
                base.fg(Color::Yellow).add_modifier(Modifier::REVERSED),
            ));
            rest = &after[end + 1..];
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
            spans.push(Span::styled(format!(" <{url}>"), base.fg(Color::DarkGray)));
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

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Modifier};

    use super::render;

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
        assert_eq!(heading.spans[0].style.fg, Some(Color::Magenta));
        assert!(heading.spans[0].style.add_modifier.contains(Modifier::BOLD));
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
        assert!(
            spans
                .iter()
                .any(|span| span.content == "cargo test" && span.style.fg == Some(Color::Yellow))
        );
        assert!(spans.iter().any(|span| {
            span.content == "Ratatui" && span.style.add_modifier.contains(Modifier::UNDERLINED)
        }));
        assert!(text(&lines[0]).contains("<https://ratatui.rs>"));
    }

    #[test]
    fn fenced_code_preserves_lines_and_has_a_gutter() {
        let lines = render("```rust\nfn main() {\n    println!(\"hi\");\n}\n```");
        let rendered: Vec<String> = lines.iter().map(text).collect();

        assert_eq!(rendered[0], "  rust");
        assert_eq!(rendered[1], "│ fn main() {");
        assert_eq!(rendered[2], "│     println!(\"hi\");");
        assert!(
            lines[1]
                .spans
                .iter()
                .all(|span| span.style.fg == Some(Color::Cyan))
        );
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
}
