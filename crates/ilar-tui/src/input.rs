//! The prompt buffer and the keys that edit it.
//!
//! A grapheme-aware multi-line buffer plus readline-style chords. Keys
//! that are not editing keys fall through to the dispatcher, which is
//! why `handle_prompt_key` reports `Unhandled` rather than swallowing
//! them.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::text::{Truncation, text_field_view_at, truncate_display};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct InputBuffer {
    text: String,
    cursor: usize,
}

impl From<&str> for InputBuffer {
    fn from(text: &str) -> Self {
        Self {
            text: text.to_string(),
            cursor: text.len(),
        }
    }
}

impl From<String> for InputBuffer {
    fn from(text: String) -> Self {
        let cursor = text.len();
        Self { text, cursor }
    }
}

impl InputBuffer {
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn is_blank(&self) -> bool {
        self.text.trim().is_empty()
    }

    pub(crate) fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    pub(crate) fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    pub(crate) fn insert(&mut self, text: &str) {
        let text = text
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .replace('\t', "    ")
            .chars()
            .filter(|character| *character == '\n' || !character.is_control())
            .collect::<String>();
        let nominal_cursor = self.cursor.saturating_add(text.len());
        self.text.insert_str(self.cursor, &text);
        self.cursor = self
            .text
            .grapheme_indices(true)
            .map(|(index, _)| index)
            .chain(std::iter::once(self.text.len()))
            .find(|boundary| *boundary >= nominal_cursor)
            .unwrap_or(self.text.len());
    }

    fn move_left(&mut self) {
        self.cursor = self.text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0);
    }

    fn move_right(&mut self) {
        if let Some(grapheme) = self.text[self.cursor..].graphemes(true).next() {
            self.cursor += grapheme.len();
        }
    }

    fn move_home(&mut self) {
        self.cursor = self.text[..self.cursor]
            .rfind('\n')
            .map(|index| index + 1)
            .unwrap_or(0);
    }

    fn move_end(&mut self) {
        self.cursor = self.text[self.cursor..]
            .find('\n')
            .map(|offset| self.cursor + offset)
            .unwrap_or(self.text.len());
    }

    fn move_vertical(&mut self, direction: isize) -> bool {
        let line_start = self.text[..self.cursor]
            .rfind('\n')
            .map(|index| index + 1)
            .unwrap_or(0);
        let line_end = self.text[self.cursor..]
            .find('\n')
            .map(|offset| self.cursor + offset)
            .unwrap_or(self.text.len());
        let (target_start, target_end) = if direction < 0 {
            if line_start == 0 {
                return false;
            }
            let end = line_start - 1;
            let start = self.text[..end]
                .rfind('\n')
                .map(|index| index + 1)
                .unwrap_or(0);
            (start, end)
        } else {
            if line_end == self.text.len() {
                return false;
            }
            let start = line_end + 1;
            let end = self.text[start..]
                .find('\n')
                .map(|offset| start + offset)
                .unwrap_or(self.text.len());
            (start, end)
        };
        let desired_column = UnicodeWidthStr::width(&self.text[line_start..self.cursor]);
        let mut column = 0usize;
        self.cursor = target_start;
        for grapheme in self.text[target_start..target_end].graphemes(true) {
            let width = UnicodeWidthStr::width(grapheme);
            if column.saturating_add(width) > desired_column {
                break;
            }
            column = column.saturating_add(width);
            self.cursor += grapheme.len();
        }
        true
    }

    fn backspace(&mut self) {
        let end = self.cursor;
        self.move_left();
        self.text.replace_range(self.cursor..end, "");
    }

    fn delete(&mut self) {
        let Some(grapheme) = self.text[self.cursor..].graphemes(true).next() else {
            return;
        };
        self.text
            .replace_range(self.cursor..self.cursor + grapheme.len(), "");
    }

    /// Kill from the cursor to the end of the visual line; at the line
    /// end, join with the next line (readline Ctrl-K).
    fn kill_to_line_end(&mut self) {
        let line_end = self.text[self.cursor..]
            .find('\n')
            .map(|offset| self.cursor + offset)
            .unwrap_or(self.text.len());
        if line_end == self.cursor {
            if self.cursor < self.text.len() {
                self.text.replace_range(self.cursor..self.cursor + 1, "");
            }
        } else {
            self.text.replace_range(self.cursor..line_end, "");
        }
    }

    /// Kill from the start of the visual line to the cursor (Ctrl-U).
    fn kill_to_line_start(&mut self) {
        let line_start = self.text[..self.cursor]
            .rfind('\n')
            .map(|index| index + 1)
            .unwrap_or(0);
        self.text.replace_range(line_start..self.cursor, "");
        self.cursor = line_start;
    }

    /// Delete the whitespace-delimited word before the cursor (Ctrl-W).
    fn delete_word_back(&mut self) {
        let head = &self.text[..self.cursor];
        let trimmed = head.trim_end_matches(|character: char| character.is_whitespace());
        let start = trimmed
            .rfind(|character: char| character.is_whitespace())
            .map(|index| index + trimmed[index..].chars().next().map_or(1, char::len_utf8))
            .unwrap_or(0);
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    fn word_char(character: char) -> bool {
        character.is_alphanumeric() || character == '_'
    }

    /// Move to the start of the previous word (Alt-B); words are
    /// alphanumeric runs, punctuation is skipped like whitespace.
    fn move_word_left(&mut self) {
        let head = &self.text[..self.cursor];
        let mut boundary = head.len();
        let mut seen_word = false;
        for (index, character) in head.char_indices().rev() {
            if Self::word_char(character) {
                seen_word = true;
                boundary = index;
            } else if seen_word {
                break;
            } else {
                boundary = index;
            }
        }
        self.cursor = if seen_word { boundary } else { 0 };
    }

    /// Move past the end of the next word (Alt-F).
    fn move_word_right(&mut self) {
        let tail = &self.text[self.cursor..];
        let mut seen_word = false;
        let mut offset = tail.len();
        for (index, character) in tail.char_indices() {
            if Self::word_char(character) {
                seen_word = true;
            } else if seen_word {
                offset = index;
                break;
            }
        }
        if !seen_word {
            offset = tail.len();
        }
        self.cursor += offset;
    }

    pub(crate) fn is_multiline(&self) -> bool {
        self.text.contains('\n')
    }

    pub(crate) fn line_count(&self) -> usize {
        self.text.bytes().filter(|byte| *byte == b'\n').count() + 1
    }

    #[cfg(test)]
    fn view(&self, width: u16) -> (String, u16) {
        text_field_view_at(&self.text, self.cursor, width)
    }

    pub(crate) fn multiline_view(&self, width: u16, height: u16) -> InputView {
        let lines = self.text.split('\n').collect::<Vec<_>>();
        let cursor_line = self.text[..self.cursor]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count();
        let cursor_line_start = self.text[..self.cursor]
            .rfind('\n')
            .map(|index| index + 1)
            .unwrap_or(0);
        let visible_count = (height as usize).max(1).min(lines.len());
        let start = cursor_line
            .saturating_add(1)
            .saturating_sub(visible_count)
            .min(lines.len().saturating_sub(visible_count));
        let mut visible = Vec::with_capacity(visible_count);
        let mut cursor_x = 0;
        for (index, line) in lines.iter().enumerate().skip(start).take(visible_count) {
            if index == cursor_line {
                let (text, offset) =
                    text_field_view_at(line, self.cursor.saturating_sub(cursor_line_start), width);
                visible.push(text);
                cursor_x = offset;
            } else {
                visible.push(truncate_display(line, width as usize, Truncation::Right));
            }
        }
        InputView {
            lines: visible,
            cursor_x,
            cursor_y: cursor_line.saturating_sub(start) as u16,
            cursor_line: cursor_line + 1,
            line_count: lines.len(),
        }
    }
}

pub(crate) struct InputView {
    pub(crate) lines: Vec<String>,
    pub(crate) cursor_x: u16,
    pub(crate) cursor_y: u16,
    pub(crate) cursor_line: usize,
    pub(crate) line_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptAction {
    Edited,
    Submit,
    Unhandled,
}

pub(crate) fn handle_prompt_key(input: &mut InputBuffer, key: KeyEvent) -> PromptAction {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
            input.insert("\n");
            PromptAction::Edited
        }
        KeyCode::Enter => PromptAction::Submit,
        KeyCode::Char('j') if control => {
            input.insert("\n");
            PromptAction::Edited
        }
        KeyCode::Char('a') if control => {
            input.move_home();
            PromptAction::Edited
        }
        KeyCode::Char('e') if control => {
            input.move_end();
            PromptAction::Edited
        }
        KeyCode::Char('k') if control => {
            input.kill_to_line_end();
            PromptAction::Edited
        }
        KeyCode::Char('u') if control => {
            input.kill_to_line_start();
            PromptAction::Edited
        }
        KeyCode::Char('w') if control => {
            input.delete_word_back();
            PromptAction::Edited
        }
        KeyCode::Char('b') if alt && !control => {
            input.move_word_left();
            PromptAction::Edited
        }
        KeyCode::Char('f') if alt && !control => {
            input.move_word_right();
            PromptAction::Edited
        }
        KeyCode::Left if !control => {
            input.move_left();
            PromptAction::Edited
        }
        KeyCode::Right if !control => {
            input.move_right();
            PromptAction::Edited
        }
        KeyCode::Home if !control => {
            input.move_home();
            PromptAction::Edited
        }
        KeyCode::End if !control => {
            input.move_end();
            PromptAction::Edited
        }
        KeyCode::Up if input.is_multiline() => {
            input.move_vertical(-1);
            PromptAction::Edited
        }
        KeyCode::Down if input.is_multiline() => {
            input.move_vertical(1);
            PromptAction::Edited
        }
        KeyCode::Backspace if !control => {
            input.backspace();
            PromptAction::Edited
        }
        KeyCode::Delete if !control => {
            input.delete();
            PromptAction::Edited
        }
        // Reachable only on a non-blank input; on a blank prompt the
        // dispatcher takes Ctrl-D as the exit.
        KeyCode::Char('d') if control => {
            input.delete();
            PromptAction::Edited
        }
        KeyCode::Char(character)
            if !key.modifiers.intersects(
                KeyModifiers::CONTROL
                    | KeyModifiers::ALT
                    | KeyModifiers::SUPER
                    | KeyModifiers::HYPER
                    | KeyModifiers::META,
            ) =>
        {
            input.insert(&character.to_string());
            PromptAction::Edited
        }
        _ => PromptAction::Unhandled,
    }
}

/// Retry is a modifier chord, never a bare letter: the prompt has focus,
/// so any printable key must reach the input buffer.
pub(crate) fn retry_requested(code: KeyCode, control: bool) -> bool {
    control && matches!(code, KeyCode::Char('r' | 'R'))
}

/// What a Ctrl-C does. It is an interrupt, never a quit — the exit is
/// Ctrl-D on a blank prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Interrupt {
    /// Whatever Esc means in the scope that is open: dismiss the overlay,
    /// else abort the running turn, else clear the input. Ctrl-C rides
    /// those paths rather than growing a second set of its own.
    AsEsc,
    /// Nothing to interrupt: point at the exit instead of doing nothing.
    Hint,
}

/// Ctrl-C aims at the innermost open scope. Only a session that is idle,
/// unobstructed and blank has nothing for it to hit. `something_open`
/// covers overlays and the armed Ctrl-X leader alike — anything Esc
/// would back out of.
pub(crate) fn interrupt(something_open: bool, busy: bool, input_blank: bool) -> Interrupt {
    if something_open || busy || !input_blank {
        Interrupt::AsEsc
    } else {
        Interrupt::Hint
    }
}

/// Ctrl-D is EOF: it quits from a blank prompt with nothing open. Its two
/// older meanings keep their scopes — delete-forward once the prompt has
/// text, and the session picker's delete confirmation inside a modal.
pub(crate) fn quit_requested(
    code: KeyCode,
    control: bool,
    has_modal: bool,
    input_blank: bool,
) -> bool {
    control && code == KeyCode::Char('d') && !has_modal && input_blank
}

/// Whether keystrokes reach the input buffer, and so whether the caret
/// and focused border should be shown. Typing during a turn is allowed —
/// it queues — so `busy` must not hide the caret.
pub(crate) fn input_accepts_keys(_busy: bool, has_modal: bool) -> bool {
    !has_modal
}

/// Inline completion candidates for a slash input: built-in commands
/// plus skills, fuzzy-ranked. Empty once the name is finished (whitespace)
/// or the input is not a slash command.
pub(crate) fn slash_candidates(
    input: &str,
    inventory: &[(String, String)],
) -> Vec<(String, String)> {
    let Some(token) = input.strip_prefix('/') else {
        return Vec::new();
    };
    if token.contains(char::is_whitespace) {
        return Vec::new();
    }
    let mut candidates: Vec<(String, String)> = vec![(
        "goal".to_string(),
        "work until the goal is achieved (evidence-based)".to_string(),
    )];
    // `goal` is built in and outranks everything, so drop anything that
    // would render as a second row with the same name.
    candidates.extend(inventory.iter().filter(|(name, _)| name != "goal").cloned());
    let mut scored: Vec<(i64, (String, String))> = candidates
        .into_iter()
        .filter_map(|(name, description)| {
            crate::text::fuzzy_score(token, &name).map(|score| (score, (name, description)))
        })
        .collect();
    scored.sort_by(|(score_a, (name_a, _)), (score_b, (name_b, _))| {
        score_b.cmp(score_a).then_with(|| name_a.cmp(name_b))
    });
    scored.into_iter().map(|(_, candidate)| candidate).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    /// A bare printable key must never trigger an action while the
    /// prompt has focus: after an error, "run the tests" began with `r`
    /// and silently resent the previous prompt as a whole new turn.
    #[test]
    fn retry_needs_a_modifier_so_letters_stay_literal() {
        assert!(retry_requested(KeyCode::Char('r'), true));
        assert!(
            !retry_requested(KeyCode::Char('r'), false),
            "a bare letter must reach the input buffer"
        );
    }

    /// Ctrl-C used to end the session from anywhere. It is an interrupt:
    /// whatever scope is open is what it aims at, innermost first.
    #[test]
    fn an_interrupt_aims_at_the_innermost_scope() {
        // An overlay outranks a running turn: Ctrl-C gets you out of the
        // picker you are looking at, not out of the turn behind it.
        assert_eq!(
            interrupt(true, true, true),
            Interrupt::AsEsc,
            "an overlay is dismissed first"
        );
        assert_eq!(interrupt(false, true, true), Interrupt::AsEsc, "abort");
        assert_eq!(
            interrupt(false, false, false),
            Interrupt::AsEsc,
            "typed text is cleared"
        );
        // An armed Ctrl-X leader is invisible except in the status line;
        // Ctrl-C must disarm it rather than hint past it and let the next
        // keystroke be swallowed as a leader argument.
        assert_eq!(
            interrupt(true, false, true),
            Interrupt::AsEsc,
            "an armed leader is something to back out of"
        );
    }

    /// Idle, blank, nothing open: the old binding would have quit here,
    /// so silence is the one answer that would be read as a broken key.
    #[test]
    fn an_interrupt_with_nothing_to_interrupt_points_at_the_exit() {
        assert_eq!(interrupt(false, false, true), Interrupt::Hint);
    }

    /// Ctrl-D is the exit, but it carries two older meanings that the
    /// quit must not shadow: delete-forward in a non-blank prompt, and
    /// the session picker's delete confirmation.
    #[test]
    fn quitting_needs_a_blank_prompt_and_no_overlay() {
        assert!(quit_requested(KeyCode::Char('d'), true, false, true));
        assert!(
            !quit_requested(KeyCode::Char('d'), true, false, false),
            "with text, Ctrl-D deletes forward"
        );
        assert!(
            !quit_requested(KeyCode::Char('d'), true, true, true),
            "in a modal, Ctrl-D is the delete confirmation"
        );
        assert!(
            !quit_requested(KeyCode::Char('d'), false, false, true),
            "a bare letter must reach the input buffer"
        );
        assert!(!quit_requested(KeyCode::Char('c'), true, false, true));
    }

    #[test]
    fn control_d_deletes_forward_like_readline() {
        let mut input = InputBuffer::from("abc");
        input.cursor = 0;
        let action = handle_prompt_key(
            &mut input,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
        );
        assert_eq!(action, PromptAction::Edited);
        assert_eq!(input.text(), "bc");
    }

    #[test]
    fn prompt_editor_is_grapheme_aware_and_inserts_at_the_cursor() {
        let mut input = InputBuffer::from("a👩‍💻b");
        input.move_left();
        input.backspace();
        input.insert("界");
        assert_eq!(input.text(), "a界b");
        input.move_left();
        input.delete();
        assert_eq!(input.text(), "ab");
        input.move_right();
        input.insert("c");
        assert_eq!(input.text(), "abc");

        let mut multiline = InputBuffer::from("first\nsecond\nthird");
        multiline.move_home();
        multiline.insert("current ");
        assert_eq!(multiline.text(), "first\nsecond\ncurrent third");
        let (visible, cursor) = multiline.view(20);
        assert_eq!(visible, "current third");
        assert_eq!(cursor, 8);

        let mut combining = InputBuffer::from("\u{301}");
        combining.move_home();
        combining.insert("a");
        combining.backspace();
        assert_eq!(combining.text(), "");
    }

    #[test]
    fn readline_chords_edit_the_current_line() {
        let chord = |input: &mut InputBuffer, code: KeyCode, modifiers: KeyModifiers| {
            assert_eq!(
                handle_prompt_key(input, KeyEvent::new(code, modifiers)),
                PromptAction::Edited
            );
        };

        // Ctrl-A / Ctrl-E are line-scoped in multiline input.
        let mut input = InputBuffer::from("first\nsecond tail");
        chord(&mut input, KeyCode::Char('a'), KeyModifiers::CONTROL);
        input.insert(">");
        assert_eq!(input.text(), "first\n>second tail");
        chord(&mut input, KeyCode::Char('e'), KeyModifiers::CONTROL);
        input.insert("<");
        assert_eq!(input.text(), "first\n>second tail<");

        // Ctrl-K kills to line end; at line end it joins the next line.
        let mut input = InputBuffer::from("keep-drop\nnext");
        input.move_vertical(-1);
        input.move_home();
        for _ in 0..4 {
            input.move_right();
        }
        chord(&mut input, KeyCode::Char('k'), KeyModifiers::CONTROL);
        assert_eq!(input.text(), "keep\nnext");
        chord(&mut input, KeyCode::Char('k'), KeyModifiers::CONTROL);
        assert_eq!(input.text(), "keepnext");

        // Ctrl-U kills to line start.
        let mut input = InputBuffer::from("first\nsecond");
        chord(&mut input, KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert_eq!(input.text(), "first\n");

        // Ctrl-W deletes the previous whitespace-delimited word.
        let mut input = InputBuffer::from("alpha beta  ");
        chord(&mut input, KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert_eq!(input.text(), "alpha ");
        chord(&mut input, KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert_eq!(input.text(), "");

        // Alt-B / Alt-F move by word across punctuation, unicode-safe.
        let mut input = InputBuffer::from("héllo, wörld");
        chord(&mut input, KeyCode::Char('b'), KeyModifiers::ALT);
        input.insert("|");
        assert_eq!(input.text(), "héllo, |wörld");
        chord(&mut input, KeyCode::Char('b'), KeyModifiers::ALT);
        chord(&mut input, KeyCode::Char('b'), KeyModifiers::ALT);
        input.insert("^");
        assert_eq!(input.text(), "^héllo, |wörld");
        chord(&mut input, KeyCode::Char('f'), KeyModifiers::ALT);
        input.insert("$");
        assert_eq!(input.text(), "^héllo$, |wörld");

        // Empty-input chords are inert, not panics.
        let mut empty = InputBuffer::default();
        for (code, modifiers) in [
            (KeyCode::Char('k'), KeyModifiers::CONTROL),
            (KeyCode::Char('u'), KeyModifiers::CONTROL),
            (KeyCode::Char('w'), KeyModifiers::CONTROL),
            (KeyCode::Char('b'), KeyModifiers::ALT),
            (KeyCode::Char('f'), KeyModifiers::ALT),
        ] {
            chord(&mut empty, code, modifiers);
            assert_eq!(empty.text(), "");
        }
    }

    #[test]
    fn paste_and_multiline_bindings_are_deliberate() {
        let mut input = InputBuffer::from("ac");
        input.move_left();
        input.insert("b\r\nsecond\rline");
        assert_eq!(input.text(), "ab\nsecond\nlinec");

        assert_eq!(
            handle_prompt_key(
                &mut input,
                KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)
            ),
            PromptAction::Edited
        );
        assert_eq!(input.text(), "ab\nsecond\nline\nc");
        let mut shifted = InputBuffer::default();
        assert_eq!(
            handle_prompt_key(
                &mut shifted,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)
            ),
            PromptAction::Edited
        );
        assert_eq!(shifted.text(), "\n");
        assert_eq!(
            handle_prompt_key(
                &mut input,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            ),
            PromptAction::Submit
        );

        let mut input = InputBuffer::from("one\ntwo\nthree");
        input.move_home();
        assert_eq!(
            handle_prompt_key(&mut input, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            PromptAction::Edited
        );
        input.insert("X");
        assert_eq!(input.text(), "one\nXtwo\nthree");
    }
}
