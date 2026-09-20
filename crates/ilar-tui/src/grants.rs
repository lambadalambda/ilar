//! The grant prompt: a tool named a stored secret, and the person
//! decides whether the command it is about to run may have it. It is
//! approval only. The [`PasswordModal`] below is the masked field the
//! other two secret questions come in: sudo's password, after a yes and
//! only where sudo wants one, and the master password of a sealed
//! store, on the first call that needs what is in it.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ilar::secrets::{Grant, GrantPrompt, PasswordPrompt, UnlockPrompt};
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
    Answer(Option<Grant>),
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

/// A modal over one prompt: what is asked, what will run, and the
/// highlighted answer. Pure — the reply channel stays with the loop.
pub(crate) struct GrantModal {
    tool: String,
    secret: String,
    description: String,
    detail: String,
    /// The asker is a child of this session, not the agent in view.
    from_subagent: bool,
    /// Which child, when the ask came from one: with several running,
    /// "(subagent)" alone does not say whose command this is.
    agent: Option<String>,
    cursor: usize,
}

impl GrantModal {
    pub(crate) fn new(prompt: &GrantPrompt, from_subagent: bool) -> Self {
        Self {
            tool: prompt.tool.clone(),
            secret: prompt.secret.clone(),
            description: prompt.description.clone(),
            detail: prompt.detail.clone(),
            from_subagent,
            agent: prompt.agent.clone(),
            cursor: 0,
        }
    }

    fn answer(&self, choice: Option<Grant>) -> GrantAction {
        GrantAction::Answer(choice)
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
        match key.code {
            KeyCode::Up if !chorded => {
                self.cursor = (self.cursor + CHOICES.len() - 1) % CHOICES.len();
            }
            KeyCode::Down if !chorded => {
                self.cursor = (self.cursor + 1) % CHOICES.len();
            }
            KeyCode::Enter if deliberate => return self.answer(CHOICES[self.cursor]),
            KeyCode::Esc if deliberate => return self.answer(None),
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
        ilar::secrets::asker_label(&self.tool, self.from_subagent, self.agent.as_deref())
    }

    /// The transcript's record of a prompt nobody is waiting on any
    /// more: the turn ended or was aborted under the modal, so the
    /// answer has nowhere to go.
    pub(crate) fn withdrawn_line(&self) -> String {
        format!(
            "grant prompt for {} withdrawn — the tool stopped waiting",
            self.secret
        )
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

    /// A row's text, with the letter that picks it: nothing types into
    /// this prompt, so every row advertises its hotkey.
    fn choice_label(&self, choice: Option<Grant>) -> String {
        match choice {
            Some(Grant::Once) => "(o) Allow once".to_string(),
            Some(Grant::Session) => "(s) Allow for this session".to_string(),
            Some(Grant::Always) => format!("(a) Always allow for {}", self.tool),
            None => "(d) Deny".to_string(),
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

    fn choice_lines(&self) -> Vec<Line<'_>> {
        CHOICES
            .iter()
            .enumerate()
            .map(|(index, choice)| {
                let pointer = if index == self.cursor { ">" } else { " " };
                let text = format!("{pointer} {}", self.choice_label(*choice));
                if index == self.cursor {
                    Line::styled(text, Style::default().add_modifier(Modifier::BOLD))
                } else {
                    Line::from(text)
                }
            })
            .collect()
    }

    pub(crate) fn render(&self, frame: &mut Frame<'_>, available: Rect) {
        if available.width == 0 || available.height == 0 {
            return;
        }
        let area = crate::modals::centered_rect(available, 76, available.height.min(24));
        let title = format!(" {} wants {} ", self.asker(), self.secret);
        let Some(inner) = crate::modals::modal_frame(frame, area, &title, theme::WAITING, FOOTER)
        else {
            return;
        };
        render_body_over(frame, inner, self.body_lines(), self.choice_lines());
    }
}

/// A prompt's two parts: the rows that must stay visible (the choices,
/// or the password field) pinned to the bottom, and the body — the
/// command above all — taking the rest. A body that does not fit says
/// so rather than hiding its tail: the part past the fold is exactly
/// where a surprise would sit.
fn render_body_over(
    frame: &mut Frame<'_>,
    inner: Rect,
    body_lines: Vec<Line<'_>>,
    rows: Vec<Line<'_>>,
) {
    let rows_height = (rows.len() as u16).min(inner.height);
    let body_height = inner.height - rows_height;
    let body = Rect::new(inner.x, inner.y, inner.width, body_height);
    if body.height > 0 {
        let paragraph = Paragraph::new(body_lines).wrap(Wrap { trim: false });
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
        Paragraph::new(rows),
        Rect::new(inner.x, body.bottom(), inner.width, rows_height),
    );
}

fn grant_word(grant: Grant) -> &'static str {
    match grant {
        Grant::Once => "once",
        Grant::Session => "this session",
        Grant::Always => "always",
    }
}

const PASSWORD_FOOTER: &str = " type or paste it · Enter send · Esc cancel ";
/// Enter on an empty field. The old prompt took that as "this system
/// needs none" and the command could never run; here it is nothing,
/// said in place, and the prompt stays up.
const PASSWORD_EMPTY: &str = "sudo needs a password on this system";
/// The same, for a store that will not open on nothing.
const UNLOCK_EMPTY: &str = "the store opens on its master password or not at all";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PasswordAction {
    Stay,
    /// What was typed; `None` cancels, and the call waiting on it —
    /// sudo's, or whichever wanted the sealed store — fails.
    Answer(Option<String>),
}

/// Which password the field is for. One prompt, two things behind it:
/// they are typed the same way and answered the same way, and only the
/// words around the field differ.
enum Wants {
    /// sudo's password for a command already approved.
    Sudo,
    /// The master password of a sealed store, wanted by the named tool
    /// before its call can go on.
    Unlock { tool: String },
}

/// The prompt a password comes in: what it is for, a masked field, and
/// nothing else to decide — the approval, where one was needed, was
/// given already.
pub(crate) struct PasswordModal {
    wants: Wants,
    detail: String,
    from_subagent: bool,
    agent: Option<String>,
    /// The last one was refused, so this is a re-ask.
    refused: bool,
    password: String,
    /// Enter came on an empty field: said in place.
    empty: bool,
}

impl PasswordModal {
    pub(crate) fn new(prompt: &PasswordPrompt, from_subagent: bool) -> Self {
        Self::of(
            Wants::Sudo,
            &prompt.detail,
            from_subagent,
            prompt.agent.as_deref(),
            prompt.refused,
        )
    }

    /// The master password of a sealed store, asked for by the call
    /// that first needs what is in it.
    pub(crate) fn unlock(prompt: &UnlockPrompt, from_subagent: bool) -> Self {
        Self::of(
            Wants::Unlock {
                tool: prompt.tool.clone(),
            },
            &prompt.detail,
            from_subagent,
            prompt.agent.as_deref(),
            prompt.refused,
        )
    }

    fn of(
        wants: Wants,
        detail: &str,
        from_subagent: bool,
        agent: Option<&str>,
        refused: bool,
    ) -> Self {
        Self {
            wants,
            detail: detail.to_string(),
            from_subagent,
            agent: agent.map(str::to_string),
            refused,
            password: String::new(),
            empty: false,
        }
    }

    /// Pasted text into the field. A password manager copies a trailing
    /// newline as often as not, and a multi-line clipboard is never one
    /// password, so the newlines are the separator that ends the paste
    /// rather than characters of it.
    pub(crate) fn paste(&mut self, text: &str) {
        let first = text.lines().next().unwrap_or_default();
        self.password
            .extend(first.chars().filter(|c| !c.is_control()));
        self.empty = false;
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> PasswordAction {
        let chorded = key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        // A key held down when the prompt opened arrives as a repeat; it
        // must not send a half-typed password or cancel the call.
        let deliberate = key.kind != KeyEventKind::Repeat;
        match key.code {
            KeyCode::Enter if deliberate => {
                if self.password.is_empty() {
                    self.empty = true;
                } else {
                    return PasswordAction::Answer(Some(self.password.clone()));
                }
            }
            KeyCode::Esc if deliberate => return PasswordAction::Answer(None),
            KeyCode::Backspace => {
                self.password.pop();
                self.empty = false;
            }
            KeyCode::Char(c) if !chorded => {
                self.password.push(c);
                self.empty = false;
            }
            _ => {}
        }
        PasswordAction::Stay
    }

    /// What the prompt calls itself, and whose tool is waiting where
    /// the name is known.
    fn title(&self) -> String {
        let what = match &self.wants {
            Wants::Sudo => "sudo password".to_string(),
            Wants::Unlock { tool } => format!("secret store password — {tool} is waiting"),
        };
        format!(
            " {} ",
            ilar::secrets::asker_label(&what, self.from_subagent, self.agent.as_deref())
        )
    }

    /// The transcript's record of a prompt nobody is waiting on any
    /// more: the turn ended or was aborted under the modal.
    pub(crate) fn withdrawn_line(&self) -> String {
        match self.wants {
            Wants::Sudo => "sudo password prompt withdrawn — the tool stopped waiting",
            Wants::Unlock { .. } => {
                "secret store password prompt withdrawn — the tool stopped waiting"
            }
        }
        .to_string()
    }

    /// The transcript's one-line record of the answer. The password
    /// itself never goes in, of course.
    pub(crate) fn outcome_line(&self, given: bool) -> String {
        match (&self.wants, given) {
            (Wants::Sudo, true) => "sudo password given",
            (Wants::Sudo, false) => "sudo password prompt cancelled — the command does not run",
            (Wants::Unlock { .. }, true) => "secret store password given",
            (Wants::Unlock { .. }, false) => {
                "the secret store stays locked — the call that needed it is refused"
            }
        }
        .to_string()
    }

    /// What the session goes back to doing once the field is answered.
    pub(crate) fn next_status(&self) -> &'static str {
        match self.wants {
            Wants::Sudo => "running sudo",
            Wants::Unlock { .. } => "opening the secret store",
        }
    }

    /// What a wrong password means, in the words of the thing that
    /// refused it.
    fn refusal(&self) -> &'static str {
        match self.wants {
            Wants::Sudo => "(sudo refused the last one)",
            Wants::Unlock { .. } => "(that did not open the store)",
        }
    }

    fn body_lines(&self) -> Vec<Line<'_>> {
        let mut lines = Vec::new();
        if self.refused {
            lines.push(Line::styled(
                self.refusal(),
                Style::default().fg(theme::ERROR),
            ));
        }
        lines.push(Line::styled("For:", Style::default().fg(theme::MUTED)));
        for row in self.detail.split('\n') {
            lines.push(Line::styled(
                format!("  {row}"),
                Style::default().add_modifier(Modifier::BOLD),
            ));
        }
        lines
    }

    fn field_lines(&self, width: usize) -> Vec<Line<'static>> {
        let mut lines = vec![masked_line("Password: ", &self.password, width)];
        if self.empty {
            let said = match self.wants {
                Wants::Sudo => PASSWORD_EMPTY,
                Wants::Unlock { .. } => UNLOCK_EMPTY,
            };
            lines.push(Line::styled(said, Style::default().fg(theme::ERROR)));
        }
        lines
    }

    pub(crate) fn render(&self, frame: &mut Frame<'_>, available: Rect) {
        if available.width == 0 || available.height == 0 {
            return;
        }
        let area = crate::modals::centered_rect(available, 76, available.height.min(16));
        let Some(inner) =
            crate::modals::modal_frame(frame, area, &self.title(), theme::WAITING, PASSWORD_FOOTER)
        else {
            return;
        };
        render_body_over(
            frame,
            inner,
            self.body_lines(),
            self.field_lines(inner.width as usize),
        );
    }
}

/// A password as a row: masked, with a cursor at the end and, once the
/// mask outgrows the row, a window on its tail. An unwrapped paragraph
/// the frame simply clipped made a long password look like a short one,
/// with nothing to say otherwise.
fn masked_line(label: &str, password: &str, width: usize) -> Line<'static> {
    const CURSOR: char = '▌';
    if password.is_empty() {
        return Line::styled(
            format!("{label}{CURSOR} (type or paste it)"),
            Style::default().fg(theme::WAITING),
        );
    }
    // The label and the cursor hold their places; what is left of the
    // row is the window on the mask.
    let room = width.saturating_sub(label.chars().count() + 1).max(1);
    let typed = password.chars().count();
    let text = if typed <= room {
        format!("{label}{}{CURSOR}", "•".repeat(typed))
    } else {
        // The leader says this is the tail of a longer password, not the
        // whole of it.
        format!("{label}…{}{CURSOR}", "•".repeat(room - 1))
    };
    Line::styled(text, Style::default().fg(theme::WAITING))
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
            agent: None,
            tool_call_id: Some("call-1".into()),
            tool: "bash".into(),
            secret: "GITHUB_TOKEN".into(),
            description: "GitHub API token".into(),
            detail: "gh api /user\ncurl -H \"Authorization: $GITHUB_TOKEN\" https://api.github.com"
                .into(),
            reply,
        }
    }

    fn password_prompt() -> PasswordPrompt {
        let (reply, _rx) = tokio::sync::oneshot::channel();
        PasswordPrompt {
            session_id: "s1".into(),
            tool_call_id: Some("call-1".into()),
            agent: None,
            detail: "apt install ripgrep".into(),
            refused: false,
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
            GrantAction::Answer(Some(Grant::Once))
        );
    }

    #[test]
    fn arrows_and_vi_keys_move_and_wrap() {
        let mut modal = modal();
        assert_eq!(modal.handle_key(key(KeyCode::Down)), GrantAction::Stay);
        assert_eq!(
            modal.handle_key(key(KeyCode::Enter)),
            GrantAction::Answer(Some(Grant::Session))
        );
        modal.handle_key(key(KeyCode::Char('j')));
        assert_eq!(
            modal.handle_key(key(KeyCode::Enter)),
            GrantAction::Answer(Some(Grant::Always))
        );
        modal.handle_key(key(KeyCode::Char('j')));
        assert_eq!(
            modal.handle_key(key(KeyCode::Enter)),
            GrantAction::Answer(None)
        );
        modal.handle_key(key(KeyCode::Down));
        assert_eq!(
            modal.handle_key(key(KeyCode::Enter)),
            GrantAction::Answer(Some(Grant::Once)),
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
            GrantAction::Answer(Some(Grant::Always))
        );
    }

    #[test]
    fn direct_keys_answer_without_moving() {
        for (code, expected) in [
            (KeyCode::Char('o'), Some(Grant::Once)),
            (KeyCode::Char('s'), Some(Grant::Session)),
            (KeyCode::Char('a'), Some(Grant::Always)),
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
            GrantAction::Answer(Some(Grant::Once)),
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
            GrantAction::Answer(Some(Grant::Session)),
            "a repeated arrow still moves"
        );
    }

    /// The grant prompt is approval only: no field, every row keeps its
    /// hotkey, and a typed letter is still a pick.
    #[test]
    fn the_grant_prompt_has_no_password_field() {
        let mut modal = modal();
        let shown = screen(&modal, 80, 24);
        assert!(!shown.contains("Password"), "{shown}");
        for prefix in ["(o)", "(s)", "(a)", "(d)"] {
            assert!(shown.contains(prefix), "{prefix} missing from {shown}");
        }
        assert_eq!(
            modal.handle_key(key(KeyCode::Char('s'))),
            GrantAction::Answer(Some(Grant::Session))
        );
    }

    /// The password prompt: the command it is for, a masked field, Enter
    /// sends what was typed, and nothing of it reaches the screen.
    #[test]
    fn the_password_prompt_types_masks_and_sends() {
        let mut modal = PasswordModal::new(&password_prompt(), false);
        let shown = password_screen(&modal, 80, 24);
        assert!(shown.contains("sudo password"), "{shown}");
        assert!(shown.contains("For:"), "{shown}");
        assert!(shown.contains("apt install ripgrep"), "{shown}");
        assert!(shown.contains("type or paste it"), "{shown}");
        assert!(!shown.contains("refused the last one"), "{shown}");
        for c in "s3cret!".chars() {
            assert_eq!(
                modal.handle_key(key(KeyCode::Char(c))),
                PasswordAction::Stay
            );
        }
        modal.handle_key(key(KeyCode::Backspace));
        modal.handle_key(key(KeyCode::Char('?')));
        let shown = password_screen(&modal, 80, 24);
        assert!(shown.contains("Password: •••••••"), "{shown}");
        assert!(!shown.contains("s3cret"), "{shown}");
        assert_eq!(
            modal.handle_key(key(KeyCode::Enter)),
            PasswordAction::Answer(Some("s3cret?".into()))
        );
        assert_eq!(modal.outcome_line(true), "sudo password given");
        assert_eq!(
            modal.withdrawn_line(),
            "sudo password prompt withdrawn — the tool stopped waiting"
        );
    }

    /// Enter on an empty field is refused in place — the old prompt took
    /// it as "this system needs none" and the command could never run.
    /// Esc is the way out, and it cancels the call.
    #[test]
    fn an_empty_password_is_refused_in_place_and_esc_cancels() {
        let mut modal = PasswordModal::new(&password_prompt(), false);
        assert_eq!(modal.handle_key(key(KeyCode::Enter)), PasswordAction::Stay);
        let shown = password_screen(&modal, 80, 24);
        assert!(shown.contains("sudo needs a password"), "{shown}");
        // Typing clears the complaint.
        modal.handle_key(key(KeyCode::Char('x')));
        assert!(
            !password_screen(&modal, 80, 24).contains("sudo needs a password"),
            "the refusal outlived the typing"
        );
        modal.handle_key(key(KeyCode::Backspace));
        assert_eq!(
            modal.handle_key(key(KeyCode::Esc)),
            PasswordAction::Answer(None)
        );
        assert_eq!(
            modal.outcome_line(false),
            "sudo password prompt cancelled — the command does not run"
        );
        // A repeat of either key — a key held down as the prompt opened
        // — answers nothing.
        let mut held = PasswordModal::new(&password_prompt(), false);
        held.paste("s3cret");
        for code in [KeyCode::Enter, KeyCode::Esc] {
            let mut repeat = key(code);
            repeat.kind = KeyEventKind::Repeat;
            assert_eq!(held.handle_key(repeat), PasswordAction::Stay, "{code:?}");
        }
    }

    /// A re-ask says sudo refused the last one; the password usually
    /// comes from a manager, so a paste lands in the field, and a mask
    /// longer than the row shows its tail rather than being clipped.
    #[test]
    fn a_re_ask_says_it_was_refused_and_a_paste_lands_in_the_field() {
        let mut modal = PasswordModal::new(
            &PasswordPrompt {
                refused: true,
                agent: Some("reviewer".into()),
                ..password_prompt()
            },
            true,
        );
        let shown = password_screen(&modal, 80, 24);
        assert!(shown.contains("(sudo refused the last one)"), "{shown}");
        assert!(
            shown.contains("sudo password (reviewer subagent)"),
            "{shown}"
        );
        modal.paste("s3cret\n");
        modal.paste("more\nignored");
        assert_eq!(
            modal.handle_key(key(KeyCode::Enter)),
            PasswordAction::Answer(Some("s3cretmore".into()))
        );

        // A password wider than the row shows its tail behind a leader.
        let mut long = PasswordModal::new(&password_prompt(), false);
        long.paste(&"x".repeat(200));
        let shown = password_screen(&long, 40, 12);
        let row = shown
            .lines()
            .find(|line| line.contains("Password:"))
            .expect("the password row");
        // Inside the frame the row is exactly label, leader, mask and
        // cursor: nothing clipped past the border, nothing left blank.
        let content = row.trim().trim_matches('║');
        assert!(content.starts_with("Password: …"), "{row}");
        assert!(content.ends_with('▌'), "{row}");
        let masked = content.chars().filter(|c| *c == '•').count();
        assert_eq!(
            masked + "Password: …▌".chars().count(),
            content.chars().count(),
            "the mask fills the row between the leader and the cursor: {row}"
        );
        assert!(masked > 4, "a window worth showing: {masked}");
    }

    /// The master password prompt is the same field with the store's
    /// words: it names the tool held up, a wrong one says the store did
    /// not open, and a cancel says the store stays locked rather than
    /// promising a command will not run.
    #[test]
    fn the_unlock_prompt_speaks_for_the_store_not_for_sudo() {
        let (reply, _rx) = tokio::sync::oneshot::channel();
        let prompt = ilar::secrets::UnlockPrompt {
            session_id: "s1".into(),
            agent: None,
            tool_call_id: Some("call-1".into()),
            tool: "bash".into(),
            detail: "gh pr list".into(),
            refused: false,
            reply,
        };
        let modal = PasswordModal::unlock(&prompt, false);
        let shown = password_screen(&modal, 80, 24);
        assert!(shown.contains("secret store"), "{shown}");
        assert!(shown.contains("bash"), "{shown}");
        assert!(shown.contains("gh pr list"), "{shown}");
        assert!(!shown.contains("sudo"), "{shown}");
        assert!(
            modal.outcome_line(false).contains("stays locked"),
            "{}",
            modal.outcome_line(false)
        );
        assert!(!modal.withdrawn_line().contains("sudo"));

        let (reply, _rx) = tokio::sync::oneshot::channel();
        let again = PasswordModal::unlock(
            &ilar::secrets::UnlockPrompt {
                refused: true,
                agent: Some("reviewer".into()),
                reply,
                ..prompt
            },
            true,
        );
        let shown = password_screen(&again, 80, 24);
        assert!(shown.contains("did not open"), "{shown}");
        assert!(shown.contains("(reviewer subagent)"), "{shown}");
    }

    /// A long command keeps the field on screen and says what it hid.
    #[test]
    fn a_long_command_keeps_the_password_field_visible() {
        let modal = PasswordModal::new(
            &PasswordPrompt {
                detail: (0..40)
                    .map(|index| format!("line {index}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                ..password_prompt()
            },
            false,
        );
        let shown = password_screen(&modal, 40, 12);
        assert_eq!(shown.lines().count(), 12);
        assert!(shown.contains("more lines not shown"), "{shown}");
        assert!(shown.contains("Password:"), "{shown}");
        // And a terminal too small for any of it does not panic.
        password_screen(&modal, 8, 3);
        password_screen(&modal, 1, 1);
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
        // With the child's name, the prompt says which of them asked.
        let named = GrantModal::new(
            &GrantPrompt {
                agent: Some("reviewer".into()),
                ..prompt()
            },
            true,
        );
        assert!(
            screen(&named, 80, 24).contains("bash (reviewer subagent) wants GITHUB_TOKEN"),
            "{}",
            screen(&named, 80, 24)
        );
        assert_eq!(
            named.outcome_line(Some(Grant::Once)),
            "GITHUB_TOKEN allowed for bash (reviewer subagent) (once)"
        );
        // The driver's own session is not a subagent, named or not.
        let root = GrantModal::new(
            &GrantPrompt {
                agent: Some("reviewer".into()),
                ..prompt()
            },
            false,
        );
        assert_eq!(root.outcome_line(None), "GITHUB_TOKEN denied for bash");
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
        assert_eq!(
            modal.withdrawn_line(),
            "grant prompt for GITHUB_TOKEN withdrawn — the tool stopped waiting"
        );
    }

    fn screen(modal: &GrantModal, width: u16, height: u16) -> String {
        shot(width, height, |frame| modal.render(frame, frame.area()))
    }

    fn password_screen(modal: &PasswordModal, width: u16, height: u16) -> String {
        shot(width, height, |frame| modal.render(frame, frame.area()))
    }

    fn shot(width: u16, height: u16, render: impl FnOnce(&mut ratatui::Frame<'_>)) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(render).unwrap();
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
