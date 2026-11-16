//! State and rendering for structured questions.

use std::cell::Cell;
use std::ops::Range;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ilar::question::{Question, QuestionAnswer, QuestionKind, QuestionRequest, QuestionResponse};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

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

impl Draft {
    /// The highlighted row of a choice draft; `None` for free text,
    /// which has rows of its own.
    fn cursor_mut(&mut self) -> Option<&mut usize> {
        match self {
            Self::Single { cursor, .. } | Self::Multiple { cursor, .. } => Some(cursor),
            Self::FreeText(_) => None,
        }
    }

    /// The "Other…" buffer a choice draft types into.
    fn other_mut(&mut self) -> Option<&mut InputBuffer> {
        match self {
            Self::Single { other, .. } | Self::Multiple { other, .. } => Some(other),
            Self::FreeText(_) => None,
        }
    }

    /// Space on an option row. The one place the two choice kinds
    /// genuinely differ: single choice replaces, multiple toggles.
    fn toggle(&mut self, index: usize) {
        match self {
            Self::Single {
                selected, other, ..
            } => {
                *selected = Some(index);
                other.clear();
            }
            Self::Multiple { selected, .. } => selected[index] = !selected[index],
            Self::FreeText(_) => {}
        }
    }

    /// Typing into "Other…" un-picks a single choice's option, since the
    /// two are mutually exclusive on the wire.
    fn note_other_edit(&mut self) {
        if let Self::Single {
            selected, other, ..
        } = self
            && !other.is_blank()
        {
            *selected = None;
        }
    }
}

/// A modal owns an immutable request and editable drafts for every question.
pub(crate) struct QuestionModal {
    request: QuestionRequest,
    drafts: Vec<Draft>,
    current: usize,
    error: Option<String>,
    // Rendering supplies the wrapped row offset and page size to input
    // handling without requiring a mutable modal at every render call.
    scroll: Cell<usize>,
    page_height: Cell<usize>,
    answer_scroll: Cell<usize>,
    answer_page_height: Cell<usize>,
    follow_answer: bool,
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
            scroll: Cell::new(0),
            page_height: Cell::new(1),
            answer_scroll: Cell::new(0),
            answer_page_height: Cell::new(1),
            follow_answer: true,
        }
    }

    #[cfg(test)]
    pub(crate) fn question_index(&self) -> usize {
        self.current
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> QuestionAction {
        self.error = None;
        // The renderer draws nothing for a request with no questions,
        // and indexing one here would panic. Close on any key rather
        // than trap the keyboard behind an invisible modal.
        if self.request.questions.is_empty() {
            return QuestionAction::Complete(QuestionResponse::Cancelled);
        }
        match key.code {
            KeyCode::Esc => return QuestionAction::Complete(QuestionResponse::Cancelled),
            KeyCode::PageUp | KeyCode::PageDown if key.modifiers.contains(KeyModifiers::SHIFT) => {
                let step = self.answer_page_height.get().saturating_sub(1).max(1);
                self.answer_scroll.set(if key.code == KeyCode::PageUp {
                    self.answer_scroll.get().saturating_sub(step)
                } else {
                    self.answer_scroll.get().saturating_add(step)
                });
                self.follow_answer = false;
                return QuestionAction::Stay;
            }
            KeyCode::PageUp | KeyCode::PageDown => {
                let step = self.page_height.get().saturating_sub(1).max(1);
                self.scroll.set(if key.code == KeyCode::PageUp {
                    self.scroll.get().saturating_sub(step)
                } else {
                    self.scroll.get().saturating_add(step)
                });
                return QuestionAction::Stay;
            }
            KeyCode::BackTab => {
                self.current = self.current.saturating_sub(1);
                self.reset_view();
                return QuestionAction::Stay;
            }
            _ => {}
        }

        self.follow_answer = true;
        let question = &self.request.questions[self.current];
        let option_count = choice_len(question);
        let allow_other = allows_other(question);

        if let Draft::FreeText(input) = &mut self.drafts[self.current] {
            return match handle_prompt_key(input, key) {
                PromptAction::Submit => self.advance(),
                PromptAction::Edited => QuestionAction::Stay,
                // At the top and bottom of the draft the arrow has no
                // row of its own to reach, so it belongs to the question
                // list — the same rescue the main prompt gives its edge
                // arrows. Moving on does not validate; Enter still does.
                PromptAction::Unhandled => {
                    self.move_question(key.code);
                    QuestionAction::Stay
                }
            };
        }

        // Single and multiple choice share every key but Space, and
        // whether typing "Other…" clears the pick.
        let draft = &mut self.drafts[self.current];
        let cursor = draft.cursor_mut().copied().unwrap_or(0);
        let on_other = allow_other && cursor == option_count;
        if on_other && !matches!(key.code, KeyCode::Up | KeyCode::Down | KeyCode::Tab) {
            match draft.other_mut().map(|other| handle_prompt_key(other, key)) {
                Some(PromptAction::Submit) => return self.advance(),
                Some(PromptAction::Edited) => {
                    draft.note_other_edit();
                    return QuestionAction::Stay;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Up | KeyCode::Down | KeyCode::Tab => {
                let delta = if key.code == KeyCode::Up { -1 } else { 1 };
                if let Some(cursor) = draft.cursor_mut() {
                    move_cursor(cursor, delta, option_count, allow_other);
                }
            }
            KeyCode::Char(' ') if cursor < option_count => draft.toggle(cursor),
            KeyCode::Enter => return self.advance(),
            _ => {}
        }
        QuestionAction::Stay
    }

    fn reset_view(&mut self) {
        self.scroll.set(0);
        self.answer_scroll.set(0);
        self.follow_answer = true;
    }

    /// Step between questions without validating — the counterpart to
    /// BackTab, for an arrow the active draft could not use.
    fn move_question(&mut self, code: KeyCode) {
        let previous = self.current;
        match code {
            KeyCode::Up => self.current = self.current.saturating_sub(1),
            KeyCode::Down if self.current + 1 < self.request.questions.len() => self.current += 1,
            _ => {}
        }
        if self.current != previous {
            self.reset_view();
        }
    }

    /// Insert bracketed-paste text when the active row accepts text.
    pub(crate) fn paste(&mut self, text: &str) {
        self.error = None;
        if self.request.questions.is_empty() {
            return;
        }
        self.follow_answer = true;
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
            self.reset_view();
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

    pub(crate) fn render(&self, frame: &mut Frame<'_>, available: Rect, newline_keys: &str) {
        if available.width == 0 || available.height == 0 || self.request.questions.is_empty() {
            return;
        }
        let question = &self.request.questions[self.current];
        // Use the terminal's width and grow vertically with wrapped content.
        // Details scroll independently; answers and keys never follow them offscreen.
        let bounds = crate::modals::centered_rect(available, available.width, available.height);
        let width = bounds.width.saturating_sub(2).max(1);
        let requirement = if question.required {
            "required"
        } else {
            "optional"
        };
        let mut details = vec![Line::styled(
            format!("{}  ({requirement})", question.prompt),
            Style::default()
                .fg(theme::PRIMARY)
                .add_modifier(Modifier::BOLD),
        )];
        if let Some(description) = &question.description {
            details.extend(
                description
                    .split('\n')
                    .map(|line| description_line(question, line)),
            );
        }
        let details = Paragraph::new(details)
            .style(Style::default().fg(theme::PRIMARY))
            .wrap(Wrap { trim: false });
        let detail_rows = details.line_count(width);
        let mut answers = Vec::new();
        let active = append_draft_lines(&mut answers, question, &self.drafts[self.current], width);
        let wrapped_rows = |end| {
            Paragraph::new(answers[..end].to_vec())
                .wrap(Wrap { trim: false })
                .line_count(width)
        };
        let active = wrapped_rows(active.start)..wrapped_rows(active.end);
        let answers = Paragraph::new(answers)
            .style(Style::default().fg(theme::PRIMARY))
            .wrap(Wrap { trim: false });
        let answer_rows = answers.line_count(width);
        let footer = Paragraph::new(question_footer(
            question,
            newline_keys,
            self.current + 1 == self.request.questions.len(),
            width,
        ))
        .style(Style::default().fg(theme::PRIMARY))
        .wrap(Wrap { trim: false });
        let footer_rows = footer.line_count(width);
        let desired_height =
            detail_rows + answer_rows + footer_rows + 3 + usize::from(self.error.is_some());
        let area = crate::modals::centered_rect(
            available,
            bounds.width,
            desired_height.min(usize::from(u16::MAX)) as u16,
        );
        let height = area.height.saturating_sub(2);
        let error_height = u16::from(self.error.is_some() && height > 1);
        let footer_height = (footer_rows as u16).min(height.saturating_sub(error_height + 2));
        let content_height = height.saturating_sub(error_height + footer_height);
        let answer_height = (answer_rows.min(usize::from(u16::MAX)) as u16)
            .min(content_height.saturating_sub(2).max(1))
            .min(content_height);
        let detail_height = content_height.saturating_sub(answer_height);
        let gap = u16::from(detail_height as usize > detail_rows);
        let detail_height = detail_height.saturating_sub(gap);
        let overflow = detail_rows > usize::from(detail_height);
        let title = if overflow && width >= 28 {
            format!(
                " question {}/{} · PgUp/PgDn ",
                self.current + 1,
                self.request.questions.len()
            )
        } else if overflow && width >= 11 {
            " PgUp/PgDn ".to_string()
        } else {
            format!(
                " question {}/{} ",
                self.current + 1,
                self.request.questions.len()
            )
        };
        let answer_hint = if answer_rows > usize::from(answer_height) {
            " Shift-PgUp/PgDn "
        } else {
            ""
        };
        let Some(inner) =
            crate::modals::modal_frame(frame, area, &title, theme::MARKUP, answer_hint)
        else {
            return;
        };
        let scroll = self
            .scroll
            .get()
            .min(detail_rows.saturating_sub(usize::from(detail_height)))
            .min(usize::from(u16::MAX));
        self.scroll.set(scroll);
        self.page_height.set(usize::from(detail_height).max(1));
        frame.render_widget(
            details.scroll((scroll as u16, 0)),
            Rect::new(inner.x, inner.y, inner.width, detail_height),
        );
        let answer_scroll = if self.follow_answer {
            // Show the whole focused choice when it fits; start at its label
            // otherwise. Shift-PgUp/PgDn can read the rest without selecting.
            active
                .end
                .saturating_sub(usize::from(answer_height))
                .min(active.start)
        } else {
            self.answer_scroll.get()
        }
        .min(answer_rows.saturating_sub(usize::from(answer_height)))
        .min(usize::from(u16::MAX));
        self.answer_scroll.set(answer_scroll);
        self.answer_page_height
            .set(usize::from(answer_height).max(1));
        let answer_y = inner.y + detail_height + gap;
        frame.render_widget(
            answers.scroll((answer_scroll as u16, 0)),
            Rect::new(inner.x, answer_y, inner.width, answer_height),
        );
        let error_y = answer_y + answer_height;
        if let Some(error) = &self.error
            && error_height > 0
        {
            frame.render_widget(
                Paragraph::new(error.as_str()).style(Style::default().fg(theme::ERROR)),
                Rect::new(inner.x, error_y, inner.width, 1),
            );
        }
        frame.render_widget(
            footer,
            Rect::new(inner.x, error_y + error_height, inner.width, footer_height),
        );
    }
}

/// Style only complete, top-level host labels. Command and reason values are
/// quoted on one physical line by the host; never scan inside those values.
/// This is presentation, not proof that a request came from the host.
fn description_line<'a>(question: &Question, line: &'a str) -> Line<'a> {
    if question.id != "sandbox_approval" {
        return Line::from(line);
    }
    let style = if line.starts_with("Command: ") || line.starts_with("Reason: ") {
        Style::default().fg(theme::SECONDARY)
    } else if [
        "Requested access (host-generated):",
        "Read paths (including descendants):",
        "Write paths (including descendants):",
        "Read paths:",
        "Write paths:",
        "All network access: true",
        "Run without ilar's sandbox: true",
    ]
    .iter()
    .any(|label| line.starts_with(label))
    {
        Style::default()
            .fg(theme::WAITING)
            .add_modifier(Modifier::BOLD)
    } else if line.starts_with("  \"") {
        Style::default()
            .fg(theme::PRIMARY)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme::PRIMARY)
    };
    Line::styled(line, style)
}

fn append_draft_lines<'a>(
    lines: &mut Vec<Line<'a>>,
    question: &'a Question,
    draft: &'a Draft,
    width: u16,
) -> Range<usize> {
    match (draft, &question.kind) {
        (Draft::FreeText(input), QuestionKind::FreeText) => append_input_lines(
            lines,
            input,
            "> ",
            width,
            Style::default().fg(theme::PRIMARY),
        ),
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
            width,
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
            width,
        ),
        _ => lines.len()..lines.len(),
    }
}

fn choice_style(focused: bool) -> Style {
    let style = Style::default().fg(theme::PRIMARY);
    if focused {
        style.bg(theme::SELECTION_BG).add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

fn append_choices<'a>(
    lines: &mut Vec<Line<'a>>,
    options: &'a [ilar::question::QuestionOption],
    cursor: usize,
    checked: impl Fn(usize) -> bool,
    allow_other: bool,
    other: &'a InputBuffer,
    width: u16,
) -> Range<usize> {
    let mut active = lines.len()..lines.len();
    for (index, option) in options.iter().enumerate() {
        let start = lines.len();
        let pointer = if cursor == index { ">" } else { " " };
        let marker = if checked(index) { "[x]" } else { "[ ]" };
        lines.push(Line::styled(
            format!("{pointer} {marker} {}", option.label),
            choice_style(cursor == index),
        ));
        if let Some(description) = &option.description {
            lines.extend(description.split('\n').map(|line| {
                Line::styled(
                    format!("      {line}"),
                    Style::default().fg(theme::SECONDARY),
                )
            }));
        }
        if cursor == index {
            active = start..lines.len();
        }
    }
    if allow_other {
        let index = options.len();
        let pointer = if cursor == index { ">" } else { " " };
        let marker = if other.is_blank() { "[ ]" } else { "[x]" };
        if cursor == index {
            return append_input_lines(
                lines,
                other,
                &format!("{pointer} {marker} "),
                width,
                choice_style(true),
            );
        }
        let other_display = if other.text().is_empty() {
            "Other…".to_string()
        } else {
            other.text().to_string()
        };
        let mut rows = other_display.split('\n').collect::<Vec<_>>();
        let first = rows.remove(0);
        lines.push(Line::styled(
            format!("{pointer} {marker} {first}"),
            choice_style(cursor == index),
        ));
        lines.extend(
            rows.into_iter()
                .map(|row| Line::from(format!("      {row}"))),
        );
    }
    active
}

/// Keep essential actions first on narrow terminals. This footer has its own
/// wrapped rows instead of being clipped inside the bottom border.
fn question_footer(question: &Question, newline_keys: &str, last: bool, width: u16) -> String {
    let enter = if last { "Enter send" } else { "Enter next" };
    let actions = match &question.kind {
        QuestionKind::FreeText if width >= 50 => format!("{enter} · {newline_keys} newline"),
        QuestionKind::FreeText => enter.to_string(),
        _ if width >= 50 => format!("↑↓ navigate · Space select · {enter}"),
        _ if width >= 28 => format!("↑↓ · Space select · {enter}"),
        _ => format!("Space · {enter}"),
    };
    if width >= 74 {
        format!("{actions} · Shift-Tab back · Esc cancel")
    } else if unicode_width::UnicodeWidthStr::width(actions.as_str()) + 13 <= usize::from(width) {
        format!("{actions} · Esc cancel")
    } else {
        format!("{actions}\nEsc cancel")
    }
}

/// Use the prompt buffer's wrapping and caret mapping. Reserve a cell for
/// the inline caret so Paragraph cannot wrap these prepared rows again.
fn append_input_lines(
    lines: &mut Vec<Line<'_>>,
    input: &InputBuffer,
    prefix: &str,
    width: u16,
    style: Style,
) -> Range<usize> {
    let indent = " ".repeat(prefix.width());
    let view = input.multiline_view(
        width.saturating_sub(prefix.width() as u16 + 1).max(1),
        u16::MAX,
    );
    let active = lines.len() + usize::from(view.cursor_y);
    for (index, mut text) in view.lines.into_iter().enumerate() {
        if index == usize::from(view.cursor_y) {
            text.insert(view.cursor_byte, '▏');
        }
        lines.push(Line::styled(
            format!("{}{text}", if index == 0 { prefix } else { &indent }),
            style,
        ));
    }
    active..active + 1
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

    /// An arrow the draft cannot use is a dead key otherwise: at the top
    /// and bottom of a free-text answer there is no row to move to, so
    /// it steps between questions instead — BackTab's counterpart, and
    /// like BackTab it does not validate.
    #[test]
    fn free_text_edge_arrows_step_between_questions() {
        let request = QuestionRequest {
            questions: (0..3)
                .map(|index| Question {
                    id: format!("q{index}"),
                    prompt: format!("Question {index}"),
                    description: None,
                    required: false,
                    kind: QuestionKind::FreeText,
                })
                .collect(),
        };
        let mut modal = QuestionModal::new(request);

        // A blank draft has no row in either direction, so both arrows
        // navigate; the ends hold rather than wrapping or submitting.
        assert_eq!(modal.handle_key(key(KeyCode::Up)), QuestionAction::Stay);
        assert_eq!(modal.question_index(), 0);
        for expected in [1, 2, 2] {
            assert_eq!(modal.handle_key(key(KeyCode::Down)), QuestionAction::Stay);
            assert_eq!(modal.question_index(), expected);
        }
        for expected in [1, 0, 0] {
            assert_eq!(modal.handle_key(key(KeyCode::Up)), QuestionAction::Stay);
            assert_eq!(modal.question_index(), expected);
        }

        // Mid-draft the arrow still belongs to the cursor.
        modal.paste("one\ntwo");
        assert_eq!(modal.handle_key(key(KeyCode::Up)), QuestionAction::Stay);
        assert_eq!(modal.question_index(), 0, "a cursor move must not navigate");
        let Draft::FreeText(input) = &modal.drafts[0] else {
            panic!("expected the free-text draft");
        };
        assert_eq!(input.text(), "one\ntwo");
        assert!(input.cursor() < 4, "the cursor moved to the first line");
        // …and from the first line the next Up navigates again.
        assert_eq!(modal.handle_key(key(KeyCode::Down)), QuestionAction::Stay);
        assert_eq!(modal.question_index(), 0, "the second line is still a row");
        assert_eq!(modal.handle_key(key(KeyCode::Down)), QuestionAction::Stay);
        assert_eq!(modal.question_index(), 1);
    }

    /// A request with no questions renders nothing; every key but Esc
    /// used to index straight past the end of the draft list.
    #[test]
    fn an_empty_request_closes_instead_of_panicking() {
        let mut modal = QuestionModal::new(QuestionRequest {
            questions: Vec::new(),
        });
        assert_eq!(
            modal.handle_key(key(KeyCode::Enter)),
            QuestionAction::Complete(QuestionResponse::Cancelled)
        );
        assert_eq!(
            modal.handle_key(key(KeyCode::Down)),
            QuestionAction::Complete(QuestionResponse::Cancelled)
        );
        modal.paste("text");
    }

    fn screen(modal: &QuestionModal, width: u16, height: u16) -> String {
        screen_with(modal, width, height, "Shift-Enter/Ctrl-J")
    }

    fn screen_with(modal: &QuestionModal, width: u16, height: u16, newline_keys: &str) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| modal.render(frame, frame.area(), newline_keys))
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

    fn approval_request(rows: usize) -> QuestionRequest {
        QuestionRequest {
            questions: vec![Question {
                id: "approval".into(),
                prompt: "Approve this request?".into(),
                description: Some(
                    (0..rows)
                        .map(|row| format!("detail {row:02}: {}", "path/".repeat(19)))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                required: true,
                kind: QuestionKind::SingleChoice {
                    allow_other: false,
                    options: ["Deny", "Allow this call only", "Allow for this session"]
                        .into_iter()
                        .enumerate()
                        .map(|(index, label)| QuestionOption {
                            id: index.to_string(),
                            label: label.into(),
                            description: (index == 2).then(|| "Reuse these read/write paths and network access in this live session only.".into()),
                        })
                        .collect(),
                },
            }],
        }
    }

    #[test]
    fn large_questions_use_terminal_space_and_readable_text() {
        let modal = QuestionModal::new(approval_request(44));
        let mut terminal = Terminal::new(TestBackend::new(160, 80)).unwrap();
        terminal
            .draw(|frame| modal.render(frame, frame.area(), "Ctrl-J"))
            .unwrap();
        let output = screen(&modal, 160, 80);
        for text in [
            "detail 00",
            "detail 43",
            "Deny",
            "Allow for this session",
            "Esc cancel",
        ] {
            assert!(output.contains(text), "missing {text}: {output}");
        }
        let buffer = terminal.backend().buffer();
        let detail = buffer
            .content
            .windows(6)
            .find(|cells| {
                cells
                    .iter()
                    .map(|cell| cell.symbol())
                    .eq(["d", "e", "t", "a", "i", "l"])
            })
            .unwrap();
        assert_eq!(detail[0].fg, theme::PRIMARY, "details must not be muted");
        let focused = buffer
            .content
            .iter()
            .find(|cell| cell.symbol() == ">")
            .unwrap();
        assert!(focused.modifier.contains(Modifier::BOLD));
        assert_eq!(focused.bg, theme::SELECTION_BG);
    }

    #[test]
    fn small_questions_keep_choices_and_cancel_visible_while_details_scroll() {
        for (width, height) in [(40, 14), (80, 23)] {
            let mut modal = QuestionModal::new(approval_request(30));
            for step in 0..5 {
                let output = screen(&modal, width, height);
                for text in [
                    "Deny",
                    "Allow this call only",
                    "Allow for this session",
                    "Esc cancel",
                    "PgUp/PgDn",
                ] {
                    assert!(
                        output.contains(text),
                        "page {step}, missing {text}: {output}"
                    );
                }
                modal.handle_key(key(KeyCode::PageDown));
            }
            modal.handle_key(key(KeyCode::Down));
            modal.handle_key(key(KeyCode::Char(' ')));
            assert!(screen(&modal, width, height).contains("> [x] Allow this call only"));
        }
    }

    #[test]
    fn compact_sandbox_approval_fits_large_terminal_and_pins_choices_on_small_terminal() {
        let mut request = approval_request(0);
        let command = format!(
            "{}printf command-end",
            "cargo test --workspace --all-features && ".repeat(18)
        );
        let read_path = "/Users/alice/.config/ilar/settings.toml";
        let write_path = "/Users/alice/.local/share/ilar/sessions";
        request.questions[0].id = "sandbox_approval".into();
        request.questions[0].prompt = "Approve this sandbox exception?".into();
        request.questions[0].description = Some(format!(
            "Tool: \"bash\"\n\nRead paths (including descendants):\n  {read_path:?}\n\nWrite paths (including descendants):\n  {write_path:?}\n\nScope: this call or this live session; not saved or shared with child sessions.\n\nReason: \"Run tests with external settings and session storage\"\nCommand: {command:?}"
        ));
        for (width, height) in [(160, 80), (80, 24)] {
            let mut modal = QuestionModal::new(request.clone());
            let output = screen(&modal, width, height);
            if width == 160 {
                for text in ["command-end", read_path, write_path, "Scope:"] {
                    assert!(output.contains(text), "missing {text}: {output}");
                }
                assert!(
                    !output.contains("PgUp/PgDn"),
                    "compact approval must fit: {output}"
                );
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal
                    .draw(|frame| {
                        modal.render(frame, frame.area(), "Ctrl-J");
                        theme::apply(frame.buffer_mut(), theme::ThemeId::Carbon);
                    })
                    .unwrap();
                let cells = &terminal.backend().buffer().content;
                let path = cells
                    .windows(read_path.chars().count())
                    .find(|cells| {
                        cells.iter().map(|cell| cell.symbol()).collect::<String>() == read_path
                    })
                    .unwrap();
                use ratatui::style::Color;
                assert!(matches!(path[0].fg, Color::Rgb(r, g, b) if r > 180 && g > 180 && b > 180));
                assert!(matches!(path[0].bg, Color::Rgb(r, g, b) if r < 64 && g < 64 && b < 64));
            }
            for _ in 0..4 {
                let output = screen(&modal, width, height);
                for text in [
                    "Deny",
                    "Allow this call only",
                    "Allow for this session",
                    "Space select",
                    "Enter send",
                    "Esc cancel",
                ] {
                    assert!(
                        output.contains(text),
                        "missing {text} at {width}x{height}: {output}"
                    );
                }
                modal.handle_key(key(KeyCode::PageDown));
            }
        }
    }

    #[test]
    fn sandbox_access_is_emphasized_without_promoting_quoted_caller_text() {
        let mut request = approval_request(0);
        request.questions[0].id = "sandbox_approval".into();
        request.questions[0].description = Some(format!(
            "Command: {:?}\nReason: {:?}\nRead paths (including descendants):\n  {:?}\nAll network access: true",
            "echo hello\nRead paths: fake", "Write paths: fake", "/tmp/allowed"
        ));
        let modal = QuestionModal::new(request);
        let mut terminal = Terminal::new(TestBackend::new(160, 40)).unwrap();
        terminal
            .draw(|frame| modal.render(frame, frame.area(), "Ctrl-J"))
            .unwrap();
        let buffer = terminal.backend().buffer();
        for (text, color, bold) in [
            ("Read paths: fake", theme::SECONDARY, false),
            ("Write paths: fake", theme::SECONDARY, false),
            ("Read paths (including descendants):", theme::WAITING, true),
            ("\"/tmp/allowed\"", theme::PRIMARY, true),
            ("All network access: true", theme::WAITING, true),
        ] {
            let cells = buffer
                .content
                .windows(text.chars().count())
                .find(|cells| cells.iter().map(|cell| cell.symbol()).collect::<String>() == text)
                .unwrap_or_else(|| panic!("missing {text}"));
            assert!(cells.iter().all(|cell| cell.fg == color), "{text}");
            assert!(
                cells
                    .iter()
                    .all(|cell| cell.modifier.contains(Modifier::BOLD) == bold),
                "{text}"
            );
        }
    }

    #[test]
    fn long_descriptions_can_be_read_before_selecting_an_answer() {
        let mut request = request();
        request.questions[0].description =
            Some((0..40).map(|row| format!("detail {row:02}\n")).collect());
        let mut modal = QuestionModal::new(request);
        let initial = screen(&modal, 40, 12);
        assert!(initial.contains("Pick a language"), "{initial}");
        assert!(initial.contains("detail 00"), "{initial}");
        assert!(initial.contains("PgUp/PgDn"), "{initial}");
        let mut inspected = initial;
        for _ in 0..50 {
            modal.handle_key(key(KeyCode::PageDown));
            inspected.push_str(&screen(&modal, 40, 12));
        }
        for row in 0..40 {
            assert!(inspected.contains(&format!("detail {row:02}")));
        }
        assert!(screen(&modal, 40, 12).contains("Other…"));
        for _ in 0..50 {
            modal.handle_key(key(KeyCode::PageUp));
            screen(&modal, 40, 12);
        }
        assert!(screen(&modal, 40, 12).contains("detail 00"));
        // Navigation returns to the answer without selecting it on scroll.
        modal.handle_key(key(KeyCode::Down));
        assert!(screen(&modal, 40, 12).contains("> [ ] Elm"));
        modal.handle_key(key(KeyCode::Char(' ')));
        modal.handle_key(key(KeyCode::Enter));
        assert_eq!(modal.question_index(), 1);
        assert!(screen(&modal, 40, 12).contains("Choose features"));
        modal.handle_key(key(KeyCode::BackTab));
        assert!(screen(&modal, 40, 12).contains("detail 00"));
    }

    #[test]
    fn paging_wrapped_descriptions_does_not_edit_free_text() {
        let mut request = request();
        let mut question = request.questions.pop().unwrap();
        question.description = Some((0..30).map(|index| format!("path{index:02} ")).collect());
        let mut modal = QuestionModal::new(QuestionRequest {
            questions: vec![question],
        });
        let mut inspected = screen(&modal, 24, 8);
        for _ in 0..40 {
            modal.handle_key(key(KeyCode::PageDown));
            inspected.push_str(&screen(&modal, 24, 8));
        }
        for index in 0..30 {
            assert!(inspected.contains(&format!("path{index:02}")));
        }
        modal.paste("unchanged");
        modal.handle_key(key(KeyCode::PageUp));
        screen(&modal, 24, 8);
        let QuestionAction::Complete(QuestionResponse::Answered { answers }) =
            modal.handle_key(key(KeyCode::Enter))
        else {
            panic!("expected the text answer");
        };
        assert!(
            matches!(&answers[0], QuestionAnswer::FreeText { text, .. } if text == "unchanged")
        );
    }

    #[test]
    fn renders_number_description_options_markers_and_footer() {
        let output = screen(&QuestionModal::new(request()), 80, 24);
        assert!(output.contains("question 1/3"));
        assert!(output.contains("Pick a language"));
        assert!(output.contains("Your draft is kept"));
        assert!(output.contains("[ ] Rust"));
        assert!(output.contains("Fast and friendly"));
        // Three questions, so the first one's Enter really does mean
        // next; the last one's says send.
        assert!(output.contains("Enter next"), "{output}");
    }

    /// The free-text footer offers Shift-Enter only where the terminal can
    /// report it. Both forms keep Esc cancel visible in the fixed footer.
    #[test]
    fn the_free_text_footer_offers_the_keys_that_arrive_and_fits() {
        let free_text = QuestionRequest {
            questions: vec![Question {
                id: "q".into(),
                prompt: "Say something".into(),
                description: None,
                required: false,
                kind: QuestionKind::FreeText,
            }],
        };
        let modal = QuestionModal::new(free_text);
        let enhanced = screen_with(&modal, 100, 24, "Shift-Enter/Ctrl-J");
        assert!(
            enhanced.contains("Shift-Enter/Ctrl-J newline"),
            "{enhanced}"
        );
        assert!(enhanced.contains("Esc cancel"), "{enhanced}");

        let plain = screen_with(&modal, 100, 24, "Ctrl-J");
        assert!(plain.contains("Ctrl-J newline"), "{plain}");
        assert!(
            !plain.contains("Shift-Enter"),
            "a key this terminal cannot send was offered: {plain}"
        );
        assert!(plain.contains("Esc cancel"), "{plain}");
    }

    #[test]
    fn text_answers_follow_wrapped_caret_after_paste_typing_and_movement() {
        for question in request().questions {
            let other = choice_len(&question);
            let mut modal = QuestionModal::new(QuestionRequest {
                questions: vec![question],
            });
            for _ in 0..other {
                modal.handle_key(key(KeyCode::Down));
            }
            modal.paste(&format!(
                "{}\n{}",
                "naïve 👩‍💻 words ".repeat(12),
                "last line ".repeat(12)
            ));
            for code in [
                None,
                Some(KeyCode::Home),
                Some(KeyCode::Left),
                Some(KeyCode::Char('z')),
            ] {
                if let Some(code) = code {
                    modal.handle_key(key(code));
                }
                let output = screen(&modal, 24, 10);
                assert!(
                    output.contains('▏'),
                    "caret hidden after {code:?}: {output}"
                );
                assert!(output.contains("Esc cancel"), "{output}");
            }
        }
    }

    #[test]
    fn final_choice_text_is_readable_without_changing_multiple_selection() {
        for oversized in [false, true] {
            let mut request = request();
            request.questions.remove(0);
            let QuestionKind::MultipleChoice {
                options,
                allow_other,
            } = &mut request.questions[0].kind
            else {
                panic!()
            };
            *allow_other = false;
            *options = (0..20)
                .map(|index| QuestionOption {
                    id: index.to_string(),
                    label: format!("Option {index}"),
                    description: None,
                })
                .collect();
            options[19].label = "Final option with a wrapped label tail".into();
            options[19].description = Some(if oversized {
                (0..12)
                    .map(|row| format!("description row {row:02}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                "Final description tail".into()
            });
            let mut modal = QuestionModal::new(request);
            modal.handle_key(key(KeyCode::Char(' ')));
            for _ in 0..19 {
                modal.handle_key(key(KeyCode::Down));
            }
            modal.handle_key(key(KeyCode::Char(' ')));
            let selected = modal.answers();
            let mut output = screen(&modal, 40, 12);
            assert!(output.contains("Final option"), "{output}");
            if oversized {
                assert!(output.contains("Shift-PgUp/PgDn"), "{output}");
                for _ in 0..15 {
                    modal.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::SHIFT));
                    output.push_str(&screen(&modal, 40, 12));
                }
                for row in 0..12 {
                    assert!(
                        output.contains(&format!("description row {row:02}")),
                        "{output}"
                    );
                }
            } else {
                assert!(output.contains("label tail"), "{output}");
                assert!(output.contains("Final description tail"), "{output}");
            }
            assert_eq!(modal.answers(), selected, "paging must not select");
            modal.handle_key(key(KeyCode::Up));
            modal.handle_key(key(KeyCode::Down));
            assert!(screen(&modal, 40, 12).contains("> [x] Final option"));
            modal.handle_key(key(KeyCode::Enter));
            modal.handle_key(key(KeyCode::BackTab));
            assert_eq!(modal.answers(), selected, "navigation must preserve picks");
            assert!(screen(&modal, 40, 12).contains("> [x] Final option"));
        }
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
        let mut modal = QuestionModal::new(request);
        let output = screen(&modal, 12, 5);
        assert_eq!(output.lines().count(), 5);
        // The shared modal chrome: a double border, like every other
        // overlay, and a margin the terminal edge cannot swallow.
        assert!(output.contains('╔') && output.contains('╝'), "{output}");
        modal.paste("a");
        let output = screen(&modal, 12, 5);
        assert!(
            output.contains('▏'),
            "active answer should become visible when editing: {output}"
        );
    }
}
