//! The grant prompt: a tool named a stored secret, and the person
//! decides whether the command it is about to run may have it.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ilar::secrets::{Approval, Grant, GrantPrompt};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

use crate::theme;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GrantAction {
    Stay,
    /// The answer; `None` denies.
    Answer(Option<Approval>),
}

/// The four answers, in the order the modal lists them. The safest
/// reversible one is first and the default.
const CHOICES: [Option<Grant>; 4] = [
    Some(Grant::Once),
    Some(Grant::Session),
    Some(Grant::Always),
    None,
];

const FOOTER: &str = " ↑↓ move · Enter choose · o once · s session · a always · d/Esc deny ";
/// With a password field the letters type; only the arrows move.
const PASSWORD_FOOTER: &str = " type the password · ↑↓ move · Enter choose · Esc deny ";

/// A modal over one prompt: what is asked, what will run, and the
/// highlighted answer. Pure — the reply channel stays with the loop.
pub(crate) struct GrantModal {
    tool: String,
    secret: String,
    description: String,
    detail: String,
    /// The asker is a child of this session, not the agent in view.
    from_subagent: bool,
    cursor: usize,
    /// What is being typed when the prompt asked for a password: sudo's,
    /// held for the session and never written. `None` when it did not.
    password: Option<String>,
}

impl GrantModal {
    pub(crate) fn new(prompt: &GrantPrompt, from_subagent: bool) -> Self {
        Self {
            tool: prompt.tool.clone(),
            secret: prompt.secret.clone(),
            description: prompt.description.clone(),
            detail: prompt.detail.clone(),
            from_subagent,
            cursor: 0,
            password: prompt.password_wanted.then(String::new),
        }
    }

    fn answer(&self, choice: Option<Grant>) -> GrantAction {
        GrantAction::Answer(choice.map(|grant| Approval {
            grant,
            password: self.password.clone().filter(|typed| !typed.is_empty()),
        }))
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> GrantAction {
        // A chorded letter is some other binding's, never a direct pick.
        let chorded = key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        // A key held down before the prompt opened must not answer it:
        // the prompt appears mid-turn, under whatever the person was
        // typing, and "always" reaches the store.
        let deliberate = key.kind != KeyEventKind::Repeat;
        // With a password field, letters are the password: the direct
        // picks and the vi keys are off, the arrows and Enter remain.
        let typing = self.password.is_some();
        match key.code {
            KeyCode::Up if !chorded => {
                self.cursor = (self.cursor + CHOICES.len() - 1) % CHOICES.len();
            }
            KeyCode::Down if !chorded => {
                self.cursor = (self.cursor + 1) % CHOICES.len();
            }
            KeyCode::Enter if deliberate => return self.answer(CHOICES[self.cursor]),
            KeyCode::Esc if deliberate => return self.answer(None),
            KeyCode::Backspace if typing => {
                if let Some(password) = &mut self.password {
                    password.pop();
                }
            }
            KeyCode::Char(c) if typing && !chorded => {
                if let Some(password) = &mut self.password {
                    password.push(c);
                }
            }
            KeyCode::Char('k') if !chorded => {
                self.cursor = (self.cursor + CHOICES.len() - 1) % CHOICES.len();
            }
            KeyCode::Char('j') if !chorded => {
                self.cursor = (self.cursor + 1) % CHOICES.len();
            }
            KeyCode::Char('o') if !chorded && deliberate => return self.answer(Some(Grant::Once)),
            KeyCode::Char('s') if !chorded && deliberate => {
                return self.answer(Some(Grant::Session));
            }
            KeyCode::Char('a') if !chorded && deliberate => {
                return self.answer(Some(Grant::Always));
            }
            KeyCode::Char('d') if !chorded && deliberate => return self.answer(None),
            _ => {}
        }
        GrantAction::Stay
    }

    /// Who is asking, for the title and the transcript line.
    fn asker(&self) -> String {
        if self.from_subagent {
            format!("{} (subagent)", self.tool)
        } else {
            self.tool.clone()
        }
    }

    /// The transcript's one-line record of the answer.
    pub(crate) fn outcome_line(&self, answer: Option<Grant>) -> String {
        match answer {
            Some(grant) => format!(
                "{} allowed for {} ({})",
                self.secret,
                self.asker(),
                grant_word(grant)
            ),
            None => format!("{} denied for {}", self.secret, self.asker()),
        }
    }

    fn choice_label(&self, choice: Option<Grant>) -> String {
        match choice {
            Some(Grant::Once) => "(o) Allow once".into(),
            Some(Grant::Session) => "(s) Allow for this session".into(),
            Some(Grant::Always) => format!("(a) Always allow for {}", self.tool),
            None => "(d) Deny".into(),
        }
    }

    /// The description and the command, verbatim. The command is what
    /// the person is saying yes to, so it gets the weight.
    fn body_lines(&self) -> Vec<Line<'_>> {
        let mut lines = Vec::new();
        if !self.description.is_empty() {
            lines.push(Line::styled(
                self.description.as_str(),
                Style::default().fg(theme::MUTED),
            ));
            lines.push(Line::default());
        }
        lines.push(Line::styled(
            "Runs, verbatim:",
            Style::default().fg(theme::MUTED),
        ));
        for row in self.detail.split('\n') {
            lines.push(Line::styled(
                format!("  {row}"),
                Style::default().add_modifier(Modifier::BOLD),
            ));
        }
        lines
    }

    /// The password row, when the prompt asked for one: masked, with
    /// the hint that empty means the system needs none.
    fn password_line(&self) -> Option<Line<'_>> {
        let password = self.password.as_ref()?;
        let text = if password.is_empty() {
            "Password: (type it here; leave empty if sudo needs none)".to_string()
        } else {
            format!("Password: {}", "•".repeat(password.chars().count()))
        };
        Some(Line::styled(text, Style::default().fg(theme::WAITING)))
    }

    fn choice_lines(&self) -> Vec<Line<'_>> {
        self.password_line()
            .into_iter()
            .chain(CHOICES.iter().enumerate().map(|(index, choice)| {
                let pointer = if index == self.cursor { ">" } else { " " };
                let text = format!("{pointer} {}", self.choice_label(*choice));
                if index == self.cursor {
                    Line::styled(text, Style::default().add_modifier(Modifier::BOLD))
                } else {
                    Line::from(text)
                }
            }))
            .collect()
    }

    pub(crate) fn render(&self, frame: &mut Frame<'_>, available: Rect) {
        if available.width == 0 || available.height == 0 {
            return;
        }
        let area = crate::modals::centered_rect(available, 76, available.height.min(24));
        let title = format!(" {} wants {} ", self.asker(), self.secret);
        let footer = if self.password.is_some() {
            PASSWORD_FOOTER
        } else {
            FOOTER
        };
        let Some(inner) = crate::modals::modal_frame(frame, area, &title, theme::WAITING, footer)
        else {
            return;
        };
        // The choices keep their rows at the bottom; the command gets
        // the rest. A command that does not fit says so rather than
        // hiding its tail — the part past the fold is exactly where a
        // surprise would sit.
        let rows = CHOICES.len() + usize::from(self.password.is_some());
        let choices_height = (rows as u16).min(inner.height);
        let body_height = inner.height - choices_height;
        let body = Rect::new(inner.x, inner.y, inner.width, body_height);
        if body.height > 0 {
            let paragraph = Paragraph::new(self.body_lines()).wrap(Wrap { trim: false });
            let total = paragraph.line_count(body.width);
            if total > usize::from(body.height) && body.height > 1 {
                let shown = body.height - 1;
                frame.render_widget(paragraph, Rect::new(body.x, body.y, body.width, shown));
                frame.render_widget(
                    Paragraph::new(format!(
                        "… {} more lines not shown",
                        total - usize::from(shown)
                    ))
                    .style(Style::default().fg(theme::ERROR)),
                    Rect::new(body.x, body.y + shown, body.width, 1),
                );
            } else {
                frame.render_widget(paragraph, body);
            }
        }
        frame.render_widget(
            Paragraph::new(self.choice_lines()),
            Rect::new(inner.x, body.bottom(), inner.width, choices_height),
        );
    }
}

fn grant_word(grant: Grant) -> &'static str {
    match grant {
        Grant::Once => "once",
        Grant::Session => "this session",
        Grant::Always => "always",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn prompt() -> GrantPrompt {
        let (reply, _rx) = tokio::sync::oneshot::channel();
        GrantPrompt {
            session_id: "s1".into(),
            tool_call_id: Some("call-1".into()),
            tool: "bash".into(),
            secret: "GITHUB_TOKEN".into(),
            description: "GitHub API token".into(),
            detail: "gh api /user\ncurl -H \"Authorization: $GITHUB_TOKEN\" https://api.github.com"
                .into(),
            password_wanted: false,
            reply,
        }
    }

    fn modal() -> GrantModal {
        GrantModal::new(&prompt(), false)
    }

    #[test]
    fn enter_picks_the_highlighted_choice_and_once_is_the_default() {
        let mut modal = modal();
        assert_eq!(
            modal.handle_key(key(KeyCode::Enter)),
            GrantAction::Answer(Some(Approval::from(Grant::Once)))
        );
    }

    #[test]
    fn arrows_and_vi_keys_move_and_wrap() {
        let mut modal = modal();
        assert_eq!(modal.handle_key(key(KeyCode::Down)), GrantAction::Stay);
        assert_eq!(
            modal.handle_key(key(KeyCode::Enter)),
            GrantAction::Answer(Some(Approval::from(Grant::Session)))
        );
        modal.handle_key(key(KeyCode::Char('j')));
        assert_eq!(
            modal.handle_key(key(KeyCode::Enter)),
            GrantAction::Answer(Some(Approval::from(Grant::Always)))
        );
        modal.handle_key(key(KeyCode::Char('j')));
        assert_eq!(
            modal.handle_key(key(KeyCode::Enter)),
            GrantAction::Answer(None)
        );
        modal.handle_key(key(KeyCode::Down));
        assert_eq!(
            modal.handle_key(key(KeyCode::Enter)),
            GrantAction::Answer(Some(Approval::from(Grant::Once))),
            "down from the last row wraps to the first"
        );
        modal.handle_key(key(KeyCode::Up));
        assert_eq!(
            modal.handle_key(key(KeyCode::Enter)),
            GrantAction::Answer(None),
            "up from the first row wraps to the last"
        );
        modal.handle_key(key(KeyCode::Char('k')));
        assert_eq!(
            modal.handle_key(key(KeyCode::Enter)),
            GrantAction::Answer(Some(Approval::from(Grant::Always)))
        );
    }

    #[test]
    fn direct_keys_answer_without_moving() {
        for (code, expected) in [
            (KeyCode::Char('o'), Some(Approval::from(Grant::Once))),
            (KeyCode::Char('s'), Some(Approval::from(Grant::Session))),
            (KeyCode::Char('a'), Some(Approval::from(Grant::Always))),
            (KeyCode::Char('d'), None),
            (KeyCode::Esc, None),
        ] {
            assert_eq!(
                modal().handle_key(key(code)),
                GrantAction::Answer(expected),
                "{code:?}"
            );
        }
    }

    #[test]
    fn chorded_letters_and_other_keys_stay() {
        let mut modal = modal();
        for code in [KeyCode::Char('a'), KeyCode::Char('d'), KeyCode::Char('j')] {
            assert_eq!(
                modal.handle_key(KeyEvent::new(code, KeyModifiers::CONTROL)),
                GrantAction::Stay,
                "{code:?}"
            );
        }
        assert_eq!(modal.handle_key(key(KeyCode::Char('x'))), GrantAction::Stay);
        assert_eq!(modal.handle_key(key(KeyCode::Tab)), GrantAction::Stay);
        assert_eq!(
            modal.handle_key(key(KeyCode::Enter)),
            GrantAction::Answer(Some(Approval::from(Grant::Once))),
            "nothing above moved the cursor"
        );
    }

    /// A key that was already held when the prompt opened arrives as a
    /// repeat; it may move the cursor but never commits an answer.
    #[test]
    fn a_repeating_key_never_answers() {
        let mut modal = modal();
        for code in [
            KeyCode::Char('a'),
            KeyCode::Char('o'),
            KeyCode::Char('d'),
            KeyCode::Enter,
            KeyCode::Esc,
        ] {
            let mut repeat = key(code);
            repeat.kind = KeyEventKind::Repeat;
            assert_eq!(modal.handle_key(repeat), GrantAction::Stay, "{code:?}");
        }
        let mut down = key(KeyCode::Down);
        down.kind = KeyEventKind::Repeat;
        modal.handle_key(down);
        assert_eq!(
            modal.handle_key(key(KeyCode::Enter)),
            GrantAction::Answer(Some(Approval::from(Grant::Session))),
            "a repeated arrow still moves"
        );
    }

    /// A prompt that wants a password turns the letters into typing:
    /// the picks come from the arrows and Enter, and the answer carries
    /// what was typed.
    #[test]
    fn a_password_prompt_types_and_hands_the_password_over() {
        let mut asking = prompt();
        asking.password_wanted = true;
        let mut modal = GrantModal::new(&asking, false);
        for c in "s3cret!".chars() {
            assert_eq!(modal.handle_key(key(KeyCode::Char(c))), GrantAction::Stay);
        }
        modal.handle_key(key(KeyCode::Backspace));
        modal.handle_key(key(KeyCode::Char('?')));
        let shown = screen(&modal, 80, 24);
        assert!(shown.contains("Password: •••••••"), "{shown}");
        assert!(!shown.contains("s3cret"), "{shown}");
        modal.handle_key(key(KeyCode::Down));
        assert_eq!(
            modal.handle_key(key(KeyCode::Enter)),
            GrantAction::Answer(Some(Approval {
                grant: Grant::Session,
                password: Some("s3cret?".into()),
            }))
        );
        // Nothing typed is no password at all.
        let mut empty = GrantModal::new(&asking, false);
        assert!(screen(&empty, 80, 24).contains("leave empty if sudo needs none"));
        assert_eq!(
            empty.handle_key(key(KeyCode::Esc)),
            GrantAction::Answer(None)
        );
        let mut empty = GrantModal::new(&asking, false);
        assert_eq!(
            empty.handle_key(key(KeyCode::Enter)),
            GrantAction::Answer(Some(Approval::from(Grant::Once)))
        );
    }

    #[test]
    fn a_subagent_asker_is_named_in_the_title_and_the_outcome() {
        let modal = GrantModal::new(&prompt(), true);
        assert!(
            screen(&modal, 80, 24).contains("bash (subagent) wants GITHUB_TOKEN"),
            "{}",
            screen(&modal, 80, 24)
        );
        assert_eq!(
            modal.outcome_line(None),
            "GITHUB_TOKEN denied for bash (subagent)"
        );
    }

    #[test]
    fn the_outcome_line_names_secret_tool_and_span() {
        let modal = modal();
        assert_eq!(
            modal.outcome_line(Some(Grant::Once)),
            "GITHUB_TOKEN allowed for bash (once)"
        );
        assert_eq!(
            modal.outcome_line(Some(Grant::Session)),
            "GITHUB_TOKEN allowed for bash (this session)"
        );
        assert_eq!(
            modal.outcome_line(Some(Grant::Always)),
            "GITHUB_TOKEN allowed for bash (always)"
        );
        assert_eq!(modal.outcome_line(None), "GITHUB_TOKEN denied for bash");
    }

    fn screen(modal: &GrantModal, width: u16, height: u16) -> String {
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
    fn renders_title_description_command_choices_and_footer() {
        let mut modal = modal();
        modal.handle_key(key(KeyCode::Down));
        let output = screen(&modal, 80, 24);
        assert!(output.contains("bash wants GITHUB_TOKEN"), "{output}");
        assert!(output.contains("GitHub API token"), "{output}");
        assert!(output.contains("Runs, verbatim:"), "{output}");
        assert!(output.contains("gh api /user"), "{output}");
        assert!(
            output.contains("curl -H \"Authorization: $GITHUB_TOKEN\""),
            "every line of the command shows: {output}"
        );
        assert!(output.contains("  (o) Allow once"), "{output}");
        assert!(output.contains("> (s) Allow for this session"), "{output}");
        assert!(output.contains("(a) Always allow for bash"), "{output}");
        assert!(output.contains("(d) Deny"), "{output}");
        assert!(output.contains("Enter choose"), "{output}");
        assert!(!output.contains("more lines not shown"), "{output}");
    }

    #[test]
    fn a_command_past_the_fold_says_so_and_the_choices_stay_visible() {
        let modal = GrantModal::new(
            &GrantPrompt {
                tool: "service".into(),
                secret: "DB_URL".into(),
                description: String::new(),
                detail: (0..40)
                    .map(|index| format!("line {index}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                ..prompt()
            },
            false,
        );
        let output = screen(&modal, 40, 12);
        assert_eq!(output.lines().count(), 12);
        assert!(output.contains("more lines not shown"), "{output}");
        assert!(output.contains("(d) Deny"), "{output}");
        assert!(output.contains('╔') && output.contains('╝'), "{output}");
    }

    #[test]
    fn rendering_is_safe_on_a_tiny_terminal() {
        let output = screen(&modal(), 8, 3);
        assert_eq!(output.lines().count(), 3);
        screen(&modal(), 1, 1);
    }
}
