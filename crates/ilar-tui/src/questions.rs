//! State and rendering for structured questions.

use crossterm::event::{KeyCode, KeyEvent};
use ilar::question::{Question, QuestionAnswer, QuestionKind, QuestionRequest, QuestionResponse};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};

use crate::input::{InputBuffer, PromptAction, handle_prompt_key};
use crate::theme;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum QuestionAction {
    Stay,
    Complete(QuestionResponse),
}

#[derive(Debug, Clone)]
enum Draft {
    Single {
        selected: Option<usize>,
        cursor: usize,
        other: InputBuffer,
    },
    Multiple {
        selected: Vec<bool>,
        cursor: usize,
        other: InputBuffer,
    },
    FreeText(InputBuffer),
}

/// A modal owns an immutable request and editable drafts for every question.
pub(crate) struct QuestionModal {
    request: QuestionRequest,
    drafts: Vec<Draft>,
    current: usize,
    error: Option<String>,
}

impl QuestionModal {
    pub(crate) fn new(request: QuestionRequest) -> Self {
        let drafts = request
            .questions
            .iter()
            .map(|question| match &question.kind {
                QuestionKind::SingleChoice { .. } => Draft::Single {
                    selected: None,
                    cursor: 0,
                    other: InputBuffer::default(),
                },
                QuestionKind::MultipleChoice { options, .. } => Draft::Multiple {
                    selected: vec![false; options.len()],
                    cursor: 0,
                    other: InputBuffer::default(),
                },
                QuestionKind::FreeText => Draft::FreeText(InputBuffer::default()),
            })
            .collect();
        Self {
            request,
            drafts,
            current: 0,
            error: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn question_index(&self) -> usize {
        self.current
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> QuestionAction {
        self.error = None;
        match key.code {
            KeyCode::Esc => return QuestionAction::Complete(QuestionResponse::Cancelled),
            KeyCode::BackTab => {
                self.current = self.current.saturating_sub(1);
                return QuestionAction::Stay;
            }
            _ => {}
        }

        let question = &self.request.questions[self.current];
        match &mut self.drafts[self.current] {
            Draft::FreeText(input) => match handle_prompt_key(input, key) {
                PromptAction::Submit => self.advance(),
                _ => QuestionAction::Stay,
            },
            Draft::Single {
                selected,
                cursor,
                other,
            } => {
                let option_count = choice_len(question);
                let on_other = allows_other(question) && *cursor == option_count;
                if on_other && !matches!(key.code, KeyCode::Up | KeyCode::Down | KeyCode::Tab) {
                    match handle_prompt_key(other, key) {
                        PromptAction::Submit => return self.advance(),
                        PromptAction::Edited => {
                            if !other.is_blank() {
                                *selected = None;
                            }
                            return QuestionAction::Stay;
                        }
                        PromptAction::Unhandled => {}
                    }
                }
                match key.code {
                    KeyCode::Up => move_cursor(cursor, -1, option_count, allows_other(question)),
                    KeyCode::Down | KeyCode::Tab => {
                        move_cursor(cursor, 1, option_count, allows_other(question))
                    }
                    KeyCode::Char(' ') if *cursor < option_count => {
                        *selected = Some(*cursor);
                        other.clear();
                    }
                    KeyCode::Enter => return self.advance(),
                    _ => {}
                }
                QuestionAction::Stay
            }
            Draft::Multiple {
                selected,
                cursor,
                other,
            } => {
                let option_count = choice_len(question);
                let on_other = allows_other(question) && *cursor == option_count;
                if on_other && !matches!(key.code, KeyCode::Up | KeyCode::Down | KeyCode::Tab) {
                    match handle_prompt_key(other, key) {
                        PromptAction::Submit => return self.advance(),
                        PromptAction::Edited => return QuestionAction::Stay,
                        PromptAction::Unhandled => {}
                    }
                }
                match key.code {
                    KeyCode::Up => move_cursor(cursor, -1, option_count, allows_other(question)),
                    KeyCode::Down | KeyCode::Tab => {
                        move_cursor(cursor, 1, option_count, allows_other(question))
                    }
                    KeyCode::Char(' ') if *cursor < option_count => {
                        selected[*cursor] = !selected[*cursor];
                    }
                    KeyCode::Enter => return self.advance(),
                    _ => {}
                }
                QuestionAction::Stay
            }
        }
    }

    /// Insert bracketed-paste text when the active row accepts text.
    pub(crate) fn paste(&mut self, text: &str) {
        self.error = None;
        match &mut self.drafts[self.current] {
            Draft::FreeText(input) => input.insert(text),
            Draft::Single {
                selected,
                cursor,
                other,
            } if allows_other(&self.request.questions[self.current])
                && *cursor == choice_len(&self.request.questions[self.current]) =>
            {
                other.insert(text);
                if !other.is_blank() {
                    *selected = None;
                }
            }
            Draft::Multiple { cursor, other, .. }
                if allows_other(&self.request.questions[self.current])
                    && *cursor == choice_len(&self.request.questions[self.current]) =>
            {
                other.insert(text);
            }
            _ => {}
        }
    }

    #[cfg(test)]
    /// Select an option row by zero-based index.
    pub(crate) fn select(&mut self, index: usize) {
        self.error = None;
        let question = &self.request.questions[self.current];
        let count = choice_len(question);
        let last = count + usize::from(allows_other(question));
        if index >= last {
            return;
        }
        match &mut self.drafts[self.current] {
            Draft::Single {
                selected,
                cursor,
                other,
            } => {
                *cursor = index;
                if index < count {
                    *selected = Some(index);
                    other.clear();
                }
            }
            Draft::Multiple {
                selected, cursor, ..
            } => {
                *cursor = index;
                if index < count {
                    selected[index] = !selected[index];
                }
            }
            Draft::FreeText(_) => {}
        }
    }

    fn advance(&mut self) -> QuestionAction {
        if let Err(error) = self.validate_current() {
            self.error = Some(error);
            return QuestionAction::Stay;
        }
        if self.current + 1 < self.request.questions.len() {
            self.current += 1;
            QuestionAction::Stay
        } else {
            let response = QuestionResponse::Answered {
                answers: self.answers(),
            };
            if let Err(error) = response.validate(&self.request) {
                self.error = Some(error.to_string());
                QuestionAction::Stay
            } else {
                QuestionAction::Complete(response)
            }
        }
    }

    fn validate_current(&self) -> Result<(), String> {
        let question = &self.request.questions[self.current];
        if !question.required {
            return Ok(());
        }
        let present = match &self.drafts[self.current] {
            Draft::Single {
                selected, other, ..
            } => selected.is_some() || !other.is_blank(),
            Draft::Multiple {
                selected, other, ..
            } => selected.iter().any(|selected| *selected) || !other.is_blank(),
            Draft::FreeText(input) => !input.is_blank(),
        };
        present
            .then_some(())
            .ok_or_else(|| "This question is required.".to_string())
    }

    fn answers(&self) -> Vec<QuestionAnswer> {
        self.request
            .questions
            .iter()
            .zip(&self.drafts)
            .filter_map(|(question, draft)| answer(question, draft))
            .collect()
    }

    pub(crate) fn render(&self, frame: &mut Frame<'_>, available: Rect) {
        if available.width == 0 || available.height == 0 || self.request.questions.is_empty() {
            return;
        }
        let area = centered_rect(available, 76, available.height.min(24));
        frame.render_widget(Clear, area);
        let title = format!(
            " Question {}/{} ",
            self.current + 1,
            self.request.questions.len()
        );
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme::FOCUS_BORDER));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let question = &self.request.questions[self.current];
        let requirement = if question.required {
            "required"
        } else {
            "optional"
        };
        let mut lines = vec![Line::styled(
            format!("{}  ({requirement})", question.prompt),
            Style::default().add_modifier(Modifier::BOLD),
        )];
        if let Some(description) = &question.description {
            lines.push(Line::styled(
                description.as_str(),
                Style::default().fg(theme::MUTED),
            ));
        }
        lines.push(Line::default());
        let active_line = append_draft_lines(&mut lines, question, &self.drafts[self.current]);

        let footer_height = u16::from(inner.height > 1);
        let error_height = u16::from(self.error.is_some() && inner.height > footer_height + 1);
        let body_height = inner.height.saturating_sub(footer_height + error_height);
        let body = Rect::new(inner.x, inner.y, inner.width, body_height);
        if body.height > 0 {
            let active_row = Paragraph::new(lines[..active_line].to_vec())
                .wrap(Wrap { trim: false })
                .line_count(body.width);
            let scroll = active_row
                .saturating_sub(body.height.saturating_sub(1) as usize)
                .min(usize::from(u16::MAX)) as u16;
            frame.render_widget(
                Paragraph::new(lines)
                    .wrap(Wrap { trim: false })
                    .scroll((scroll, 0)),
                body,
            );
        }
        if let Some(error) = &self.error
            && error_height > 0
        {
            frame.render_widget(
                Paragraph::new(error.as_str()).style(Style::default().fg(theme::ERROR)),
                Rect::new(inner.x, body.bottom(), inner.width, 1),
            );
        }
        if footer_height > 0 {
            frame.render_widget(
                Paragraph::new(question_footer(question)).style(Style::default().fg(theme::MUTED)),
                Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
            );
        }
    }
}

fn append_draft_lines<'a>(
    lines: &mut Vec<Line<'a>>,
    question: &'a Question,
    draft: &'a Draft,
) -> usize {
    match (draft, &question.kind) {
        (Draft::FreeText(input), QuestionKind::FreeText) => {
            let active = lines.len();
            let display = text_with_caret(input);
            for (index, text) in display.split('\n').enumerate() {
                lines.push(Line::from(format!(
                    "{}{}",
                    if index == 0 { "> " } else { "  " },
                    text
                )));
            }
            active
        }
        (
            Draft::Single {
                selected,
                cursor,
                other,
            },
            QuestionKind::SingleChoice {
                allow_other,
                options,
            },
        ) => append_choices(
            lines,
            options,
            *cursor,
            |index| *selected == Some(index),
            *allow_other,
            other,
        ),
        (
            Draft::Multiple {
                selected,
                cursor,
                other,
            },
            QuestionKind::MultipleChoice {
                allow_other,
                options,
            },
        ) => append_choices(
            lines,
            options,
            *cursor,
            |index| selected[index],
            *allow_other,
            other,
        ),
        _ => lines.len(),
    }
}

fn append_choices<'a>(
    lines: &mut Vec<Line<'a>>,
    options: &'a [ilar::question::QuestionOption],
    cursor: usize,
    checked: impl Fn(usize) -> bool,
    allow_other: bool,
    other: &'a InputBuffer,
) -> usize {
    let mut active = lines.len();
    for (index, option) in options.iter().enumerate() {
        if cursor == index {
            active = lines.len();
        }
        let pointer = if cursor == index { ">" } else { " " };
        let marker = if checked(index) { "[x]" } else { "[ ]" };
        lines.push(Line::from(format!("{pointer} {marker} {}", option.label)));
        if let Some(description) = &option.description {
            lines.push(Line::styled(
                format!("      {description}"),
                Style::default().fg(theme::MUTED),
            ));
        }
    }
    if allow_other {
        let index = options.len();
        if cursor == index {
            active = lines.len();
        }
        let pointer = if cursor == index { ">" } else { " " };
        let marker = if other.is_blank() { "[ ]" } else { "[x]" };
        let other_display = if cursor == index {
            text_with_caret(other)
        } else if other.text().is_empty() {
            "Other…".to_string()
        } else {
            other.text().to_string()
        };
        let mut rows = other_display.split('\n').collect::<Vec<_>>();
        let first = rows.remove(0);
        lines.push(Line::from(format!("{pointer} {marker} {first}")));
        lines.extend(
            rows.into_iter()
                .map(|row| Line::from(format!("      {row}"))),
        );
    }
    active
}

fn question_footer(question: &Question) -> &'static str {
    match &question.kind {
        QuestionKind::FreeText => {
            "Enter next  Shift-Enter/Ctrl-J newline  BackTab back  Esc cancel"
        }
        QuestionKind::SingleChoice { .. } | QuestionKind::MultipleChoice { .. } => {
            "↑↓ navigate  Space select  Enter next  BackTab back  Esc cancel"
        }
    }
}

fn text_with_caret(input: &InputBuffer) -> String {
    let mut display = input.text().to_string();
    display.insert(input.cursor(), '▏');
    display
}

fn answer(question: &Question, draft: &Draft) -> Option<QuestionAnswer> {
    match draft {
        Draft::Single {
            selected, other, ..
        } => {
            let other = (!other.is_blank()).then(|| other.text().to_string());
            let option_id = if other.is_some() {
                None
            } else {
                selected.and_then(|index| {
                    choice_options(question)
                        .get(index)
                        .map(|option| option.id.clone())
                })
            };
            (option_id.is_some() || other.is_some()).then(|| QuestionAnswer::SingleChoice {
                question_id: question.id.clone(),
                option_id,
                other,
            })
        }
        Draft::Multiple {
            selected, other, ..
        } => {
            let option_ids = choice_options(question)
                .iter()
                .zip(selected)
                .filter(|(_, selected)| **selected)
                .map(|(option, _)| option.id.clone())
                .collect::<Vec<_>>();
            let other = (!other.is_blank()).then(|| other.text().to_string());
            (!option_ids.is_empty() || other.is_some()).then(|| QuestionAnswer::MultipleChoice {
                question_id: question.id.clone(),
                option_ids,
                other,
            })
        }
        Draft::FreeText(input) => (!input.is_blank()).then(|| QuestionAnswer::FreeText {
            question_id: question.id.clone(),
            text: input.text().to_string(),
        }),
    }
}

fn choice_options(question: &Question) -> &[ilar::question::QuestionOption] {
    match &question.kind {
        QuestionKind::SingleChoice { options, .. }
        | QuestionKind::MultipleChoice { options, .. } => options,
        QuestionKind::FreeText => &[],
    }
}

fn choice_len(question: &Question) -> usize {
    choice_options(question).len()
}

fn allows_other(question: &Question) -> bool {
    match &question.kind {
        QuestionKind::SingleChoice { allow_other, .. }
        | QuestionKind::MultipleChoice { allow_other, .. } => *allow_other,
        QuestionKind::FreeText => false,
    }
}

fn move_cursor(cursor: &mut usize, delta: isize, options: usize, other: bool) {
    let count = options + usize::from(other);
    if count > 0 {
        *cursor = (*cursor as isize + delta).rem_euclid(count as isize) as usize;
    }
}

fn centered_rect(area: Rect, max_width: u16, max_height: u16) -> Rect {
    let width = area.width.min(max_width).max(1);
    let height = area.height.min(max_height).max(1);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use ilar::question::QuestionOption;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn request() -> QuestionRequest {
        QuestionRequest {
            questions: vec![
                Question {
                    id: "language".into(),
                    prompt: "Pick a language".into(),
                    description: Some("Your draft is kept when moving back.".into()),
                    required: true,
                    kind: QuestionKind::SingleChoice {
                        allow_other: true,
                        options: vec![
                            QuestionOption {
                                id: "rust".into(),
                                label: "Rust".into(),
                                description: Some("Fast and friendly".into()),
                            },
                            QuestionOption {
                                id: "elm".into(),
                                label: "Elm".into(),
                                description: None,
                            },
                        ],
                    },
                },
                Question {
                    id: "features".into(),
                    prompt: "Choose features".into(),
                    description: None,
                    required: true,
                    kind: QuestionKind::MultipleChoice {
                        allow_other: true,
                        options: vec![
                            QuestionOption {
                                id: "tests".into(),
                                label: "Tests".into(),
                                description: None,
                            },
                            QuestionOption {
                                id: "docs".into(),
                                label: "Docs".into(),
                                description: None,
                            },
                        ],
                    },
                },
                Question {
                    id: "notes".into(),
                    prompt: "Notes".into(),
                    description: None,
                    required: true,
                    kind: QuestionKind::FreeText,
                },
            ],
        }
    }

    #[test]
    fn required_question_blocks_advance_and_backtab_preserves_draft() {
        let mut modal = QuestionModal::new(request());
        assert_eq!(modal.handle_key(key(KeyCode::Down)), QuestionAction::Stay);
        assert_eq!(
            modal.handle_key(key(KeyCode::Char(' '))),
            QuestionAction::Stay
        );
        assert_eq!(modal.handle_key(key(KeyCode::Enter)), QuestionAction::Stay);
        assert_eq!(modal.question_index(), 1);
        modal.select(0);
        assert_eq!(modal.handle_key(key(KeyCode::Enter)), QuestionAction::Stay);
        assert_eq!(modal.question_index(), 2);
        assert_eq!(modal.handle_key(key(KeyCode::Enter)), QuestionAction::Stay);
        assert_eq!(modal.error.as_deref(), Some("This question is required."));
        modal.handle_key(key(KeyCode::BackTab));
        modal.handle_key(key(KeyCode::BackTab));
        assert_eq!(modal.question_index(), 0);
        assert!(matches!(
            modal.drafts[0],
            Draft::Single {
                selected: Some(1),
                ..
            }
        ));
    }

    #[test]
    fn multiple_choice_toggles_and_unicode_text_submits_all_answers() {
        let mut modal = QuestionModal::new(request());
        modal.select(0);
        modal.handle_key(key(KeyCode::Enter));
        modal.handle_key(key(KeyCode::Char(' ')));
        modal.handle_key(key(KeyCode::Down));
        modal.handle_key(key(KeyCode::Char(' ')));
        modal.handle_key(key(KeyCode::Enter));
        modal.paste("naïve 👩‍💻");
        let action = modal.handle_key(key(KeyCode::Enter));
        let QuestionAction::Complete(QuestionResponse::Answered { answers }) = action else {
            panic!("expected answers");
        };
        assert_eq!(answers.len(), 3);
        assert!(
            matches!(&answers[1], QuestionAnswer::MultipleChoice { option_ids, .. } if option_ids == &["tests", "docs"])
        );
        assert!(matches!(&answers[2], QuestionAnswer::FreeText { text, .. } if text == "naïve 👩‍💻"));
    }

    #[test]
    fn selecting_an_option_clears_single_choice_other_text() {
        let mut request = request();
        request.questions.truncate(1);
        let mut modal = QuestionModal::new(request);
        modal.select(2);
        modal.paste("Zig");
        modal.select(0);
        let QuestionAction::Complete(QuestionResponse::Answered { answers }) =
            modal.handle_key(key(KeyCode::Enter))
        else {
            panic!()
        };
        assert!(
            matches!(&answers[0], QuestionAnswer::SingleChoice { option_id: Some(id), other: None, .. } if id == "rust")
        );
    }

    #[test]
    fn optional_single_choice_can_be_skipped_with_enter() {
        let request = QuestionRequest {
            questions: vec![Question {
                id: "optional".into(),
                prompt: "Optional".into(),
                description: None,
                required: false,
                kind: QuestionKind::SingleChoice {
                    allow_other: false,
                    options: vec![QuestionOption {
                        id: "one".into(),
                        label: "One".into(),
                        description: None,
                    }],
                },
            }],
        };
        let mut modal = QuestionModal::new(request);
        assert_eq!(
            modal.handle_key(key(KeyCode::Enter)),
            QuestionAction::Complete(QuestionResponse::Answered { answers: vec![] })
        );
    }

    #[test]
    fn optional_blank_answers_are_omitted_and_escape_cancels() {
        let request = QuestionRequest {
            questions: vec![Question {
                id: "optional".into(),
                prompt: "Optional".into(),
                description: None,
                required: false,
                kind: QuestionKind::FreeText,
            }],
        };
        let mut modal = QuestionModal::new(request);
        assert_eq!(
            modal.handle_key(key(KeyCode::Enter)),
            QuestionAction::Complete(QuestionResponse::Answered { answers: vec![] })
        );
        assert_eq!(
            modal.handle_key(key(KeyCode::Esc)),
            QuestionAction::Complete(QuestionResponse::Cancelled)
        );
    }

    fn screen(modal: &QuestionModal, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| modal.render(frame, frame.area()))
            .unwrap();
        terminal.backend().buffer().content.iter().enumerate().fold(
            String::new(),
            |mut output, (index, cell)| {
                output.push_str(cell.symbol());
                if (index + 1) % width as usize == 0 {
                    output.push('\n');
                }
                output
            },
        )
    }

    #[test]
    fn renders_number_description_options_markers_and_footer() {
        let output = screen(&QuestionModal::new(request()), 80, 24);
        assert!(output.contains("Question 1/3"));
        assert!(output.contains("Pick a language"));
        assert!(output.contains("Your draft is kept"));
        assert!(output.contains("[ ] Rust"));
        assert!(output.contains("Fast and friendly"));
        assert!(output.contains("Enter next"));
    }

    #[test]
    fn rendering_is_safe_on_tiny_terminal_and_long_unicode_content() {
        let request = QuestionRequest {
            questions: vec![Question {
                id: "q".into(),
                prompt: "非常に長い質問 👩‍💻 without convenient breaks xxxxxxxxxxxxxxxxxxxxxxxxx"
                    .into(),
                description: Some("also long".repeat(20)),
                required: true,
                kind: QuestionKind::FreeText,
            }],
        };
        let output = screen(&QuestionModal::new(request), 12, 5);
        assert_eq!(output.lines().count(), 5);
        assert!(output.contains('╭') && output.contains('╯'));
        assert!(
            output.contains('▏'),
            "active answer should remain visible: {output}"
        );
    }
}
