//! ilar TUI: transcript, streaming, tool display, input. Esc aborts.

mod diff;
mod highlight;
mod history;
mod markdown;
mod modals;
mod session_view;
mod text;
mod theme;
mod transcript;

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseButton,
    MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::supports_keyboard_enhancement;
use modals::{
    CommandPalette, CommandPaletteAction, Modal, ModelPicker, PendingAction, PendingItem,
    PendingManager, PickerAction, SessionPicker, SessionPickerAction, SkillPicker, ThemePicker,
    VariantPicker, VariantPickerAction, activate_palette_command, apply_theme_picker_action,
    is_command_palette_shortcut, palette_items, render_command_palette, render_help,
    render_model_picker, render_pending_manager, render_session_picker, render_skill_picker,
    render_theme_picker, render_variant_picker,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, Padding, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState,
};
use ratatui::{Frame, buffer::Buffer};
use session_view::{
    accrue_usage, restored_session_view_with_store, task_notification_display,
    tool_notification_display,
};
use text::{
    Truncation, abbreviated_path, bounded_detail, context_meter, context_usage, format_bytes,
    format_cost, format_tokens_compact, fuzzy_score, safe_lines, safe_text, styled_graphemes,
    styled_line, text_field_view_at, truncate_display, wrap_styled_line,
};
use tokio_util::sync::CancellationToken;
use transcript::{
    Line_, ToolKind, ToolProgress, ToolState, TranscriptHitTarget, TranscriptRenderCache,
    append_thought_tail, apply_subagent_activity, toggle_tool_expansion,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use ilar::agent::{
    LOOP_EVENT_CAPACITY, LoopConfig, LoopEvent, LoopEventReceiver, TurnOutcome, loop_event_channel,
    run_turn,
};
use ilar::config::{Loader, system_prompt_for};
use ilar::provider::ProviderResolver;
use ilar::session::{SessionMeta, SessionStore, new_id};
use ilar::subagent::SubagentSpawner;
use ilar::tools::{ToolContext, ToolRegistry};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Activity {
    Ready,
    Thinking,
    Responding,
    Tools,
    Aborting,
    Aborted,
    Stopped,
    Paused,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoticeLevel {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StatusNotice {
    text: String,
    level: NoticeLevel,
    persistent: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct SelectionPoint {
    row: usize,
    column: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TranscriptSelection {
    anchor: SelectionPoint,
    focus: SelectionPoint,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RenderedCell {
    Character(char),
    Text(String),
    Space,
    Continuation { lead: usize },
}

type RenderedRow = Vec<RenderedCell>;

impl TranscriptSelection {
    fn ordered(self) -> (SelectionPoint, SelectionPoint) {
        if self.anchor <= self.focus {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        }
    }
}

const MUTED: Color = theme::MUTED;
const ASSISTANT: Color = theme::ASSISTANT;
const TOOL_ACTIVE: Color = theme::RUNNING;
const ERROR: Color = theme::ERROR;
const CONTENT_HORIZONTAL_PADDING: u16 = 2;
const TODO_SIDEBAR_MIN_WIDTH: u16 = 121;
const TODO_SIDEBAR_WIDTH: u16 = 42;
const TODO_SIDEBAR_MAX_ITEMS: usize = 5;
const MAX_WHEEL_EVENTS_PER_BATCH: usize = 1024;
/// Show "no data Ns" in the status line once the stream has been silent
/// this long during thinking/responding.
const STREAM_STALL_AFTER: std::time::Duration = std::time::Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ContentAreas {
    transcript: Rect,
    todos: Option<Rect>,
}

fn content_areas(area: Rect) -> ContentAreas {
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

#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Log in to OpenAI with your ChatGPT account (OAuth in the browser)
    Login,
}

#[derive(Parser, Debug)]
#[command(name = "ilar", version, about = "personal coding agent")]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    /// Model to use (provider/model-id); overrides config.
    #[arg(long)]
    model: Option<String>,

    /// Session id to resume.
    #[arg(long)]
    session: Option<String>,

    /// Resume the most recently modified session.
    #[arg(long = "continue", conflicts_with = "session")]
    continue_last: bool,

    /// Agent name from config (markdown agents).
    #[arg(long)]
    agent: Option<String>,

    /// Print the resolved system prompt and exit (debugging).
    #[arg(long)]
    print_prompt: bool,
}

struct TerminalSession {
    terminal_initialized: bool,
    keyboard_enhanced: bool,
    mouse_enabled: bool,
    paste_enabled: bool,
}

impl TerminalSession {
    fn start() -> Result<(ratatui::DefaultTerminal, Self)> {
        let mut session = Self {
            terminal_initialized: false,
            keyboard_enhanced: false,
            mouse_enabled: false,
            paste_enabled: false,
        };
        let terminal = match ratatui::try_init() {
            Ok(terminal) => terminal,
            Err(error) => {
                ratatui::restore();
                return Err(error.into());
            }
        };
        session.terminal_initialized = true;

        if supports_keyboard_enhancement().unwrap_or(false) {
            session.keyboard_enhanced = true;
            crossterm::execute!(
                std::io::stdout(),
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            )?;
        }

        session.mouse_enabled = true;
        crossterm::execute!(std::io::stdout(), EnableMouseCapture)?;
        session.paste_enabled = true;
        if let Err(error) = crossterm::execute!(std::io::stdout(), EnableBracketedPaste) {
            if error.kind() == std::io::ErrorKind::Unsupported {
                let _ = crossterm::execute!(std::io::stdout(), DisableBracketedPaste);
                session.paste_enabled = false;
            } else {
                return Err(error.into());
            }
        }
        Ok((terminal, session))
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if self.paste_enabled {
            let _ = crossterm::execute!(std::io::stdout(), DisableBracketedPaste);
        }
        if self.mouse_enabled {
            let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
        }
        if self.keyboard_enhanced {
            let _ = crossterm::execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
        }
        if self.terminal_initialized {
            ratatui::restore();
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct InputBuffer {
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
    fn text(&self) -> &str {
        &self.text
    }

    fn is_blank(&self) -> bool {
        self.text.trim().is_empty()
    }

    fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    fn insert(&mut self, text: &str) {
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

    fn is_multiline(&self) -> bool {
        self.text.contains('\n')
    }

    fn line_count(&self) -> usize {
        self.text.bytes().filter(|byte| *byte == b'\n').count() + 1
    }

    #[cfg(test)]
    fn view(&self, width: u16) -> (String, u16) {
        text_field_view_at(&self.text, self.cursor, width)
    }

    fn multiline_view(&self, width: u16, height: u16) -> InputView {
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

struct InputView {
    lines: Vec<String>,
    cursor_x: u16,
    cursor_y: u16,
    cursor_line: usize,
    line_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptAction {
    Edited,
    Submit,
    Unhandled,
}

fn handle_prompt_key(input: &mut InputBuffer, key: KeyEvent) -> PromptAction {
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
        // Reachable only on a non-blank input; the dispatcher keeps
        // Ctrl-D as half-page scroll while the prompt is empty.
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

/// "12.3 KiB" while data flows, "12.3 KiB · no data Ns" once the stream
/// has been silent past the stall threshold. `None` before any turn.
fn stream_liveness(
    received: u64,
    last_data: Option<std::time::Instant>,
    rate: Option<f64>,
    now: std::time::Instant,
) -> Option<String> {
    let since = now.saturating_duration_since(last_data?);
    Some(if since >= STREAM_STALL_AFTER {
        format!("{} · no data {}s", format_bytes(received), since.as_secs())
    } else {
        match rate {
            Some(rate) if rate >= 1.0 => format!(
                "{} · {}/s",
                format_bytes(received),
                format_bytes(rate as u64)
            ),
            _ => format_bytes(received),
        }
    })
}

/// Advance a >=1s measurement window; returns the completed window's
/// bytes/sec when one elapses.
fn windowed_rate(
    anchor: &mut Option<(std::time::Instant, u64)>,
    received: u64,
    now: std::time::Instant,
) -> Option<f64> {
    match *anchor {
        None => {
            *anchor = Some((now, received));
            None
        }
        Some((window_start, window_bytes)) => {
            let elapsed = now.saturating_duration_since(window_start);
            if elapsed < std::time::Duration::from_secs(1) {
                return None;
            }
            *anchor = Some((now, received));
            Some(received.saturating_sub(window_bytes) as f64 / elapsed.as_secs_f64())
        }
    }
}

fn activity_line(
    busy: bool,
    activity: Activity,
    now: std::time::Instant,
    activity_started: std::time::Instant,
    liveness: Option<&str>,
) -> Option<Line<'static>> {
    if !busy
        || !matches!(
            activity,
            Activity::Thinking | Activity::Responding | Activity::Tools
        )
    {
        return None;
    }
    let elapsed = now.saturating_duration_since(activity_started);
    let (frame, label, color) = match activity {
        Activity::Thinking => {
            let frames = ["◐", "◓", "◑", "◒"];
            (
                frames[(elapsed.as_millis() / 160) as usize % frames.len()],
                "thinking…",
                theme::REASONING,
            )
        }
        Activity::Responding => {
            let frames = ["▏", "▎", "▍", "▎"];
            (
                frames[(elapsed.as_millis() / 120) as usize % frames.len()],
                "responding…",
                ASSISTANT,
            )
        }
        Activity::Tools => {
            let frames = ["◐", "◓", "◑", "◒"];
            (
                frames[(elapsed.as_millis() / 160) as usize % frames.len()],
                "processing tools and agents…",
                TOOL_ACTIVE,
            )
        }
        _ => unreachable!(),
    };
    let label = match liveness {
        // Tool rows carry their own progress; liveness belongs to the
        // provider-stream states only.
        Some(liveness) if !matches!(activity, Activity::Tools) => {
            format!("{label} · {liveness}")
        }
        _ => label.to_string(),
    };
    Some(Line::from(vec![
        Span::styled(
            "ilar ",
            Style::default().fg(ASSISTANT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("{frame} "), Style::default().fg(color)),
        Span::styled(label, Style::default().fg(MUTED)),
    ]))
}

struct App {
    lines: Vec<Line_>,
    input: InputBuffer,
    history: history::PromptHistory,
    busy: bool,
    status: String,
    notice: Option<StatusNotice>,
    activity: Activity,
    activity_started: std::time::Instant,
    current_model: String,
    current_variant: Option<String>,
    session_id: String,
    cwd: std::path::PathBuf,
    context_used: u64,
    context_limit: Option<u64>,
    context_estimated: bool,
    latest_usage: Option<ilar::session::Usage>,
    session_usage: ilar::session::Usage,
    session_cost: Option<f64>,
    /// Bytes of streamed text/thinking received this turn, plus the last
    /// arrival instant — the status line's stream-liveness indicator.
    stream_received: u64,
    stream_last_data: Option<std::time::Instant>,
    /// Bytes already attributed to completed steps; the live output
    /// estimate uses only the current step's bytes.
    stream_step_base: u64,
    /// Windowed transfer rate: anchor of the current >=1s window and the
    /// last completed window's bytes/sec.
    stream_rate_anchor: Option<(std::time::Instant, u64)>,
    stream_rate: Option<f64>,
    /// Last submitted prompt, offered for Ctrl-R retry after a turn error.
    last_prompt: Option<String>,
    retry_available: bool,
    /// Messages submitted during an active turn, auto-sent in order when
    /// the turn completes.
    queued_messages: Vec<String>,
    /// Active goal: (description, completed rounds). Turns auto-continue
    /// until the model emits GOAL_ACHIEVED or the round cap trips.
    goal: Option<(String, u32)>,
    /// Selection inside the inline slash-completion popup.
    slash_selected: usize,
    pending_manager: Option<PendingManager>,
    /// Snapshot of spawner.running_background() for rendering.
    background_running: usize,
    /// Snapshot of the service manager's running count for rendering.
    services_running: usize,
    /// (name, running, detail) rows for the sidebar.
    services_view: Vec<(String, bool, String)>,
    /// Palette-requested compaction, applied to the next turn's config.
    compact_requested: bool,
    search_active: bool,
    search_query: String,
    search_matches: Vec<usize>,
    search_current: usize,
    /// (scroll_top, follow_tail) before the search opened; Esc restores.
    search_saved: Option<(usize, bool)>,
    search_computed_revision: Option<u64>,
    scroll_top: usize,
    content_rows: usize,
    viewport_rows: usize,
    follow_tail: bool,
    command_palette: Option<CommandPalette>,
    model_picker: Option<ModelPicker>,
    variant_picker: Option<VariantPicker>,
    session_picker: Option<SessionPicker>,
    /// Set by the palette; run_app opens the picker (it owns the store).
    session_picker_requested: bool,
    help_visible: bool,
    help_scroll: usize,
    skill_picker: Option<SkillPicker>,
    /// (name, description) pairs for slash invocation and the picker.
    skills: Vec<(String, String)>,
    theme: theme::ThemeId,
    theme_picker: Option<ThemePicker>,
    /// Whether the terminal speaks the kitty keyboard protocol. Without
    /// it Ctrl-M is indistinguishable from Enter, so the help overlay
    /// must not advertise it.
    keyboard_enhanced: bool,
    model_key_pending: bool,
    transcript_text_area: Rect,
    transcript_cache: TranscriptRenderCache,
    transcript_hit_targets: Vec<Option<TranscriptHitTarget>>,
    transcript_cells: Vec<RenderedRow>,
    transcript_selection: Option<TranscriptSelection>,
    selecting_transcript: bool,
    transcript_dragged: bool,
    clipboard: Option<arboard::Clipboard>,
    next_tool_group: u64,
    next_thought: u64,
    expanded_tool_groups: std::collections::HashSet<String>,
    transcript_revision: u64,
    pending_subagent_activity: std::collections::VecDeque<ilar::subagent::SubagentActivity>,
    todos: std::sync::Arc<std::sync::Mutex<ilar::todo::TodoList>>,
}

impl App {
    fn new() -> Self {
        Self {
            lines: vec![Line_::System(
                "ilar — Enter sends, Shift-Enter/Ctrl-J newline, Ctrl-P commands, PgUp/PgDn scroll"
                    .into(),
            )],
            input: InputBuffer::default(),
            history: history::PromptHistory::in_memory(),
            busy: false,
            status: String::new(),
            notice: None,
            activity: Activity::Ready,
            activity_started: std::time::Instant::now(),
            current_model: "unknown".into(),
            current_variant: None,
            session_id: String::new(),
            cwd: std::path::PathBuf::from("."),
            context_used: 0,
            context_limit: None,
            context_estimated: true,
            latest_usage: None,
            session_usage: ilar::session::Usage::default(),
            session_cost: Some(0.0),
            stream_received: 0,
            stream_last_data: None,
            stream_step_base: 0,
            stream_rate_anchor: None,
            stream_rate: None,
            last_prompt: None,
            retry_available: false,
            queued_messages: Vec::new(),
            goal: None,
            slash_selected: 0,
            pending_manager: None,
            background_running: 0,
            services_running: 0,
            services_view: Vec::new(),
            compact_requested: false,
            search_active: false,
            search_query: String::new(),
            search_matches: Vec::new(),
            search_current: 0,
            search_saved: None,
            search_computed_revision: None,
            scroll_top: 0,
            content_rows: 0,
            viewport_rows: 0,
            follow_tail: true,
            command_palette: None,
            model_picker: None,
            variant_picker: None,
            session_picker: None,
            session_picker_requested: false,
            help_visible: false,
            help_scroll: 0,
            skill_picker: None,
            skills: Vec::new(),
            theme: theme::ThemeId::Terminal,
            theme_picker: None,
            keyboard_enhanced: false,
            model_key_pending: false,
            transcript_text_area: Rect::default(),
            transcript_cache: TranscriptRenderCache::default(),
            transcript_hit_targets: Vec::new(),
            transcript_cells: Vec::new(),
            transcript_selection: None,
            selecting_transcript: false,
            transcript_dragged: false,
            clipboard: None,
            next_tool_group: 0,
            next_thought: 0,
            expanded_tool_groups: std::collections::HashSet::new(),
            transcript_revision: 0,
            pending_subagent_activity: std::collections::VecDeque::new(),
            todos: std::sync::Arc::new(std::sync::Mutex::new(ilar::todo::TodoList::default())),
        }
    }

    fn open_command_palette(&mut self) {
        if !self.busy && !self.has_modal() {
            self.model_key_pending = false;
            self.clear_transient_notice();
            self.command_palette = Some(CommandPalette::new(palette_items()));
        }
    }

    /// The overlay that owns the keyboard, in one precedence order.
    ///
    /// Render and key dispatch both derive from this. They used to keep
    /// separate, near-opposite orders by hand; nothing opened two
    /// overlays at once, so the app drew and typed into the same one by
    /// luck rather than by construction.
    fn active_modal(&self) -> Option<Modal> {
        if self.pending_manager.is_some() {
            Some(Modal::PendingManager)
        } else if self.help_visible {
            Some(Modal::Help)
        } else if self.theme_picker.is_some() {
            Some(Modal::ThemePicker)
        } else if self.skill_picker.is_some() {
            Some(Modal::SkillPicker)
        } else if self.session_picker.is_some() {
            Some(Modal::SessionPicker)
        } else if self.model_picker.is_some() {
            Some(Modal::ModelPicker)
        } else if self.variant_picker.is_some() {
            Some(Modal::VariantPicker)
        } else if self.search_active {
            Some(Modal::Search)
        } else if self.command_palette.is_some() {
            Some(Modal::CommandPalette)
        } else {
            None
        }
    }

    fn has_modal(&self) -> bool {
        self.active_modal().is_some()
    }

    /// Route a wheel batch to whatever overlay is in front. Returns
    /// false when nothing consumed it, so the transcript can scroll.
    fn scroll_active_modal(&mut self, rows: isize) -> bool {
        // A net-zero batch must fall through: `scroll_wheel` is what
        // clears a stale transcript selection.
        if rows == 0 {
            return false;
        }
        // Every `move_selection` wraps with `rem_euclid`, so one call with
        // the whole delta does what a per-row loop would.
        match self.active_modal() {
            Some(Modal::ModelPicker) => self.model_picker.as_mut().unwrap().move_selection(rows),
            Some(Modal::VariantPicker) => {
                self.variant_picker.as_mut().unwrap().move_selection(rows);
            }
            Some(Modal::ThemePicker) => {
                self.theme_picker.as_mut().unwrap().move_selection(rows);
                // The picker previews the highlighted theme live and its
                // footer advertises it, so the wheel must preview too.
                self.theme = self.theme_picker.as_ref().unwrap().selected_theme();
            }
            Some(Modal::SessionPicker) => {
                self.session_picker.as_mut().unwrap().move_selection(rows);
            }
            Some(Modal::SkillPicker) => self.skill_picker.as_mut().unwrap().move_selection(rows),
            Some(Modal::CommandPalette) => {
                self.command_palette.as_mut().unwrap().move_selection(rows);
            }
            Some(Modal::Help) => {
                self.help_scroll = self.help_scroll.saturating_add_signed(rows);
            }
            // The pending manager is a handful of rows; search leaves the
            // wheel to the transcript so results stay browsable.
            Some(Modal::PendingManager) | Some(Modal::Search) | None => return false,
        }
        true
    }

    fn configure_runtime(
        &mut self,
        model: String,
        variant: Option<String>,
        cwd: std::path::PathBuf,
        context_used: u64,
        context_limit: Option<u64>,
        context_estimated: bool,
    ) {
        self.current_model = model;
        self.current_variant = variant;
        self.cwd = cwd;
        self.context_used = context_used;
        self.context_limit = context_limit;
        self.context_estimated = context_estimated;
        self.status = "ready".into();
        self.notice = None;
    }

    fn restore_session(&mut self, session: &ilar::session::SessionReader, store: &SessionStore) {
        let restored = restored_session_view_with_store(session, store);
        self.lines.extend(restored.lines);
        self.latest_usage = restored.latest_usage;
        self.session_usage = restored.total_usage;
        self.session_cost = restored.total_cost;
        self.transcript_revision = self.transcript_revision.wrapping_add(1);
    }

    fn allocate_thought_id(&mut self) -> String {
        self.next_thought = self.next_thought.wrapping_add(1);
        format!("thought:{}", self.next_thought)
    }

    /// Mark any open thought complete (its phase ended: content or tools
    /// started arriving).
    fn close_open_thought(&mut self) {
        if let Some(Line_::Thought { complete, .. }) = self.lines.iter_mut().rev().find(|line| {
            matches!(
                line,
                Line_::Thought {
                    complete: false,
                    ..
                }
            )
        }) {
            *complete = true;
        }
    }

    fn note_stream_data(&mut self, bytes: usize) {
        let now = std::time::Instant::now();
        self.stream_received = self.stream_received.saturating_add(bytes as u64);
        self.stream_last_data = Some(now);
        if let Some(rate) = windowed_rate(&mut self.stream_rate_anchor, self.stream_received, now) {
            self.stream_rate = Some(rate);
        }
    }

    fn set_activity(&mut self, activity: Activity) {
        if self.activity != activity {
            // Entering a streaming state restarts the liveness clock so a
            // long tool phase doesn't immediately read as a stalled stream.
            if matches!(activity, Activity::Thinking | Activity::Responding)
                && self.stream_last_data.is_some()
            {
                self.stream_last_data = Some(std::time::Instant::now());
            }
            self.activity = activity;
            self.activity_started = std::time::Instant::now();
        }
    }

    fn push_transcript_line(&mut self, line: Line_) {
        self.lines.push(line);
        self.transcript_revision = self.transcript_revision.wrapping_add(1);
    }

    fn push_notification(&mut self, description: &str, text: &str) {
        self.transcript_revision = self.transcript_revision.wrapping_add(1);
        if let Some(text) = task_notification_display(text) {
            let id = self.allocate_thought_id();
            self.lines.push(Line_::Task {
                id,
                text,
                expanded: false,
            });
        } else if let Some(text) = tool_notification_display(text) {
            let id = self.allocate_thought_id();
            self.lines.push(Line_::Job {
                id,
                text,
                expanded: false,
            });
        } else {
            self.lines
                .push(Line_::System(format!("task notification: {description}")));
            self.lines.push(Line_::User(text.to_string()));
        }
    }

    fn push_loop_event(&mut self, event: &LoopEvent) {
        self.transcript_revision = self.transcript_revision.wrapping_add(1);
        match event {
            LoopEvent::TurnStarted => {
                self.clear_transient_notice();
                self.status = "thinking…".into();
                self.stream_received = 0;
                self.stream_step_base = 0;
                self.stream_rate_anchor = None;
                self.stream_rate = None;
                // Seed liveness at turn start: a provider that hangs
                // before its first byte must still show "0 B · no data Ns"
                // instead of a bare spinner.
                self.stream_last_data = Some(std::time::Instant::now());
                self.set_activity(Activity::Thinking);
            }
            LoopEvent::TextDelta(t) => {
                self.note_stream_data(t.len());
                self.close_open_thought();
                self.status = "responding".into();
                self.set_activity(Activity::Responding);
                match self.lines.last_mut() {
                    Some(Line_::Assistant(text)) => text.push_str(t),
                    _ => self.lines.push(Line_::Assistant(t.clone())),
                }
            }
            LoopEvent::ThinkingDelta(delta) => {
                self.note_stream_data(delta.len());
                // Raw thinking accumulates into an expandable Thought line
                // (bounded to a tail) so the user can watch it live.
                match self.lines.last_mut() {
                    Some(Line_::Thought {
                        text,
                        complete: false,
                        ..
                    }) => append_thought_tail(text, delta),
                    _ => {
                        let id = self.allocate_thought_id();
                        self.lines.push(Line_::Thought {
                            id,
                            text: delta.clone(),
                            complete: false,
                            expanded: false,
                        });
                    }
                }
                self.status = "thinking".into();
                self.set_activity(Activity::Thinking);
            }
            LoopEvent::ReasoningSummaryDelta(summary) => {
                self.note_stream_data(summary.len());
                self.status = "thinking".into();
                self.set_activity(Activity::Thinking);
                match self.lines.last_mut() {
                    Some(Line_::Thought {
                        text,
                        complete: false,
                        ..
                    }) => append_thought_tail(text, summary),
                    _ => {
                        let id = self.allocate_thought_id();
                        self.lines.push(Line_::Thought {
                            id,
                            text: summary.clone(),
                            complete: false,
                            expanded: false,
                        });
                    }
                }
            }
            LoopEvent::ReasoningSummaryCompleted => {
                if let Some(complete) = self.lines.iter_mut().rev().find_map(|line| match line {
                    Line_::Thought { complete, .. } if !*complete => Some(complete),
                    _ => None,
                }) {
                    *complete = true;
                }
            }
            LoopEvent::ToolStarted { id, name } => {
                self.close_open_thought();
                self.lines.push(Line_::Tool {
                    id: id.clone(),
                    group_id: format!("live:{}", self.next_tool_group),
                    name: name.clone(),
                    kind: ToolKind::Tool,
                    arguments: String::new(),
                    argument_detail: String::new(),
                    diff: Vec::new(),
                    tail: String::new(),
                    result: None,
                    state: ToolState::Running,
                    progress: ToolProgress::None,
                    expanded: false,
                    full: false,
                    child_lines: Vec::new(),
                    child_group: 0,
                    child_running: false,
                    child_session_id: None,
                });
                self.status = format!("running {name}");
                self.set_activity(Activity::Tools);
            }
            LoopEvent::ToolArguments {
                id,
                arguments: summary,
            } => {
                if let Some(arguments) = self.lines.iter_mut().rev().find_map(|line| match line {
                    Line_::Tool {
                        id: line_id,
                        arguments,
                        ..
                    } if line_id == id => Some(arguments),
                    _ => None,
                }) {
                    *arguments = summary.clone();
                }
            }
            LoopEvent::ToolInputProgress {
                id,
                received_bytes,
                last_data,
            } => {
                self.stream_last_data = Some(*last_data);
                if let Some(progress) = self.lines.iter_mut().rev().find_map(|line| match line {
                    Line_::Tool {
                        id: line_id,
                        state: ToolState::Running,
                        progress,
                        ..
                    } if line_id == id => Some(progress),
                    _ => None,
                }) && !matches!(
                    progress,
                    ToolProgress::Queued | ToolProgress::Executing { .. }
                ) {
                    *progress = ToolProgress::Receiving {
                        received_bytes: *received_bytes,
                        last_data: *last_data,
                    };
                }
            }
            LoopEvent::ToolInputComplete { id, arguments } => {
                if let Some((name, progress, detail, diff)) =
                    self.lines.iter_mut().rev().find_map(|line| match line {
                        Line_::Tool {
                            id: line_id,
                            name,
                            state: ToolState::Running,
                            progress,
                            argument_detail,
                            diff,
                            ..
                        } if line_id == id => Some((name, progress, argument_detail, diff)),
                        _ => None,
                    })
                {
                    *progress = ToolProgress::Queued;
                    *detail = bounded_detail(arguments);
                    *diff = diff::tool_diff(name, arguments);
                }
            }
            LoopEvent::SubagentConfigured {
                id,
                description,
                agent,
                model,
            } => {
                if let Some((kind, arguments)) =
                    self.lines.iter_mut().rev().find_map(|line| match line {
                        Line_::Tool {
                            id: line_id,
                            kind,
                            arguments,
                            ..
                        } if line_id == id => Some((kind, arguments)),
                        _ => None,
                    })
                {
                    *kind = ToolKind::Agent {
                        name: agent.clone(),
                        model: model.clone(),
                    };
                    *arguments = description.clone();
                }
            }
            LoopEvent::ToolExecutionStarted {
                id,
                received_bytes,
                started,
            } => {
                if let Some(progress) = self.lines.iter_mut().rev().find_map(|line| match line {
                    Line_::Tool {
                        id: line_id,
                        state: ToolState::Running,
                        progress,
                        ..
                    } if line_id == id => Some(progress),
                    _ => None,
                }) {
                    *progress = ToolProgress::Executing {
                        received_bytes: *received_bytes,
                        started: *started,
                    };
                }
            }
            LoopEvent::ToolOutputTail { id, tail } => {
                if let Some(current) = self.lines.iter_mut().rev().find_map(|line| match line {
                    Line_::Tool {
                        id: line_id,
                        state: ToolState::Running,
                        tail,
                        ..
                    } if line_id == id => Some(tail),
                    _ => None,
                }) {
                    *current = tail.clone();
                }
            }
            LoopEvent::ToolExecutionCompleted { id } => {
                if let Some((state, progress)) =
                    self.lines.iter_mut().rev().find_map(|line| match line {
                        Line_::Tool {
                            id: line_id,
                            state,
                            progress,
                            ..
                        } if line_id == id && *state == ToolState::Running => {
                            Some((state, progress))
                        }
                        _ => None,
                    })
                {
                    *state = ToolState::Complete;
                    *progress = ToolProgress::None;
                }
            }
            LoopEvent::ToolFinished {
                id,
                name,
                is_error,
                result,
                child_session_id,
            } => {
                let mut matched = false;
                if let Some((state, progress, stored_result, stored_child_session)) =
                    self.lines.iter_mut().rev().find_map(|line| match line {
                        Line_::Tool {
                            id: line_id,
                            state,
                            progress,
                            result,
                            child_session_id,
                            ..
                        } if line_id == id
                            && matches!(*state, ToolState::Running | ToolState::Complete) =>
                        {
                            Some((state, progress, result, child_session_id))
                        }
                        _ => None,
                    })
                {
                    *state = if *is_error {
                        ToolState::Failed
                    } else {
                        ToolState::Succeeded
                    };
                    *progress = ToolProgress::None;
                    *stored_result = Some(bounded_detail(result));
                    *stored_child_session = child_session_id.clone();
                    matched = true;
                }
                if !matched {
                    self.lines.push(Line_::Tool {
                        id: id.clone(),
                        group_id: format!("live:{}", self.next_tool_group),
                        name: name.clone(),
                        kind: ToolKind::Tool,
                        arguments: String::new(),
                        argument_detail: String::new(),
                        diff: Vec::new(),
                        tail: String::new(),
                        result: Some(bounded_detail(result)),
                        state: if *is_error {
                            ToolState::Failed
                        } else {
                            ToolState::Succeeded
                        },
                        progress: ToolProgress::None,
                        expanded: false,
                        full: false,
                        child_lines: Vec::new(),
                        child_group: 0,
                        child_running: false,
                        child_session_id: child_session_id.clone(),
                    });
                }
                let running = self
                    .lines
                    .iter()
                    .filter(|line| {
                        matches!(
                            line,
                            Line_::Tool {
                                state: ToolState::Running,
                                ..
                            }
                        )
                    })
                    .count();
                self.status = match running {
                    0 => "thinking".into(),
                    1 => "running 1 tool".into(),
                    count => format!("running {count} tools"),
                };
                if running == 0 {
                    self.set_activity(Activity::Thinking);
                }
            }
            LoopEvent::StepComplete { stop_reason, usage } => {
                self.stream_step_base = self.stream_received;
                self.next_tool_group = self.next_tool_group.saturating_add(1);
                self.latest_usage = Some(*usage);
                let model = self.current_model.clone();
                accrue_usage(
                    &mut self.session_usage,
                    &mut self.session_cost,
                    &model,
                    usage,
                );
                let reported = usage.context_tokens();
                if reported > 0 {
                    self.context_used = reported;
                    self.context_estimated = false;
                } else {
                    self.context_estimated = true;
                }
                self.status = format!(
                    "{stop_reason} · in {} out {} (request cache read {} / write {})",
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.cache_read_input_tokens,
                    usage.cache_creation_input_tokens
                );
            }
            LoopEvent::Compacted {
                context_tokens,
                summary,
            } => {
                self.context_used = *context_tokens;
                self.context_estimated = true;
                self.lines
                    .push(Line_::System(format!("transcript compacted\n{summary}")));
            }
            LoopEvent::TurnDone { outcome } => {
                self.lines.retain(|line| {
                    !matches!(
                        line,
                        Line_::Thought {
                            complete: false,
                            ..
                        }
                    )
                });
                if *outcome == TurnOutcome::Aborted {
                    for line in &mut self.lines {
                        if let Line_::Tool { state, .. } = line
                            && matches!(*state, ToolState::Running | ToolState::Complete)
                        {
                            *state = ToolState::Failed;
                        }
                    }
                }
                self.status = match outcome {
                    TurnOutcome::Completed => "ready".into(),
                    TurnOutcome::Aborted => "aborted".into(),
                    TurnOutcome::MaxIterations => "stopped: max iterations".into(),
                };
                match outcome {
                    TurnOutcome::Completed => self.clear_transient_notice(),
                    TurnOutcome::Aborted => self.set_notice("turn aborted", NoticeLevel::Warning),
                    TurnOutcome::MaxIterations => {
                        self.set_notice("stopped: max iterations", NoticeLevel::Warning)
                    }
                }
                self.set_activity(match outcome {
                    TurnOutcome::Completed => Activity::Ready,
                    TurnOutcome::Aborted => Activity::Aborted,
                    TurnOutcome::MaxIterations => Activity::Stopped,
                });
            }
        }
    }

    fn push_subagent_activity(&mut self, activity: &ilar::subagent::SubagentActivity) {
        self.transcript_revision = self.transcript_revision.wrapping_add(1);
        if !apply_subagent_activity(&mut self.lines, &self.session_id, activity)
            && self.pending_subagent_activity.len() < 256
        {
            self.pending_subagent_activity.push_back(activity.clone());
        }
        self.retry_subagent_activity();
    }

    fn retry_subagent_activity(&mut self) {
        let pending = self.pending_subagent_activity.len();
        for _ in 0..pending {
            let Some(activity) = self.pending_subagent_activity.pop_front() else {
                break;
            };
            if !apply_subagent_activity(&mut self.lines, &self.session_id, &activity) {
                self.pending_subagent_activity.push_back(activity);
            }
        }
    }

    fn finish_turn(&mut self, result: anyhow::Result<TurnOutcome>) {
        self.transcript_revision = self.transcript_revision.wrapping_add(1);
        self.retry_subagent_activity();
        if let Err(error) = result {
            self.lines.retain(|line| {
                !matches!(
                    line,
                    Line_::Thought {
                        complete: false,
                        ..
                    }
                )
            });
            for line in &mut self.lines {
                if let Line_::Tool { state, .. } = line
                    && matches!(*state, ToolState::Running | ToolState::Complete)
                {
                    *state = ToolState::Failed;
                }
            }
            let mut message = format!("error: {error:#}");
            self.lines.push(Line_::System(message.clone()));
            if self.last_prompt.is_some() {
                self.retry_available = true;
                message.push_str(" — Ctrl-R to retry");
            }
            self.set_notice(&message, NoticeLevel::Error);
            self.status = "error".into();
            self.set_activity(Activity::Error);
        }
        self.busy = false;
    }

    fn pending_items(&self) -> Vec<PendingItem> {
        let mut items: Vec<PendingItem> = (0..self.queued_messages.len())
            .map(PendingItem::Queued)
            .collect();
        if self.goal.is_some() {
            items.push(PendingItem::Goal);
        }
        if self.background_running > 0 {
            items.push(PendingItem::BackgroundJobs);
        }
        if self.services_running > 0 {
            items.push(PendingItem::Services);
        }
        if self.retry_available {
            items.push(PendingItem::Retry);
        }
        items
    }

    fn pending_manager_key(&mut self, code: KeyCode, control: bool) -> PendingAction {
        let items = self.pending_items();
        let Some(manager) = self.pending_manager.as_mut() else {
            return PendingAction::Stay;
        };
        if items.is_empty() {
            return match code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => PendingAction::Close,
                _ => PendingAction::Stay,
            };
        }
        manager.selected = manager.selected.min(items.len() - 1);
        let selected = items[manager.selected];
        match (code, control) {
            (KeyCode::Esc, _) | (KeyCode::Char('q'), true | false) => PendingAction::Close,
            (KeyCode::Up, _) | (KeyCode::Char('p'), true) => {
                manager.selected = (manager.selected + items.len() - 1) % items.len();
                manager.armed = None;
                PendingAction::Stay
            }
            (KeyCode::Down, _) | (KeyCode::Char('n'), true) => {
                manager.selected = (manager.selected + 1) % items.len();
                manager.armed = None;
                PendingAction::Stay
            }
            (KeyCode::Delete | KeyCode::Backspace | KeyCode::Char('d'), _) => {
                match selected {
                    // Removing one queued message is targeted enough to
                    // fire immediately.
                    PendingItem::Queued(index) => PendingAction::DeleteQueued(index),
                    PendingItem::Retry => PendingAction::DismissRetry,
                    // Goal and background jobs are investments: confirm.
                    armed_item => {
                        if manager.armed == Some(armed_item) {
                            manager.armed = None;
                            match armed_item {
                                PendingItem::Goal => PendingAction::AbortGoal,
                                PendingItem::BackgroundJobs => PendingAction::CancelBackground,
                                PendingItem::Services => PendingAction::StopServices,
                                _ => PendingAction::Stay,
                            }
                        } else {
                            manager.armed = Some(armed_item);
                            PendingAction::Stay
                        }
                    }
                }
            }
            (KeyCode::Enter, _) => match selected {
                PendingItem::Queued(index) => PendingAction::EditQueued(index),
                PendingItem::Goal => PendingAction::EditGoal,
                PendingItem::Retry => PendingAction::RetryNow,
                PendingItem::BackgroundJobs | PendingItem::Services => PendingAction::Stay,
            },
            _ => PendingAction::Stay,
        }
    }

    fn open_search(&mut self) {
        self.search_active = true;
        if self.search_saved.is_none() {
            self.search_saved = Some((self.scroll_top, self.follow_tail));
        }
        self.search_refresh();
    }

    /// Recompute matches against the cached rows and jump to the first.
    fn search_refresh(&mut self) {
        self.search_matches = self.transcript_cache.matching_rows(&self.search_query);
        self.search_computed_revision = Some(self.transcript_revision);
        self.search_current = 0;
        if !self.search_matches.is_empty() {
            self.search_scroll_to_current();
        }
    }

    fn search_jump(&mut self, delta: isize) {
        let count = self.search_matches.len();
        if count == 0 {
            return;
        }
        self.search_current =
            (self.search_current as isize + delta).rem_euclid(count as isize) as usize;
        self.search_scroll_to_current();
    }

    fn search_scroll_to_current(&mut self) {
        let Some(&row) = self.search_matches.get(self.search_current) else {
            return;
        };
        self.follow_tail = false;
        self.scroll_top = row
            .saturating_sub(self.viewport_rows / 3)
            .min(self.max_scroll());
    }

    fn close_search(&mut self, restore_scroll: bool) {
        self.search_active = false;
        if let Some((scroll_top, follow_tail)) = self.search_saved.take()
            && restore_scroll
        {
            self.scroll_top = scroll_top.min(self.max_scroll());
            self.follow_tail = follow_tail;
        }
        if !restore_scroll {
            self.search_matches.clear();
        }
    }

    fn max_scroll(&self) -> usize {
        self.content_rows.saturating_sub(self.viewport_rows)
    }

    fn page_size(&self) -> usize {
        self.viewport_rows.saturating_sub(2).max(1)
    }

    fn scroll_up(&mut self, rows: usize) {
        self.clear_transcript_selection();
        self.follow_tail = false;
        self.scroll_top = self.scroll_top.saturating_sub(rows);
    }

    fn scroll_down(&mut self, rows: usize) {
        self.clear_transcript_selection();
        let max_scroll = self.max_scroll();
        self.scroll_top = self.scroll_top.saturating_add(rows).min(max_scroll);
        self.follow_tail = self.scroll_top == max_scroll;
    }

    fn scroll_wheel(&mut self, rows: isize) {
        self.clear_transcript_selection();
        if rows < 0 {
            self.scroll_up(rows.unsigned_abs());
        } else if rows > 0 {
            self.scroll_down(rows as usize);
        }
    }

    fn scroll_to_top(&mut self) {
        self.clear_transcript_selection();
        self.scroll_top = 0;
        self.follow_tail = self.max_scroll() == 0;
    }

    fn scroll_to_tail(&mut self) {
        self.clear_transcript_selection();
        self.scroll_top = self.max_scroll();
        self.follow_tail = true;
    }

    fn update_scroll_metrics(&mut self, content_rows: usize, viewport_rows: usize) {
        self.content_rows = content_rows;
        self.viewport_rows = viewport_rows;
        let max_scroll = self.max_scroll();
        if self.follow_tail {
            self.scroll_top = max_scroll;
        } else {
            self.scroll_top = self.scroll_top.min(max_scroll);
        }
    }

    fn clear_transcript_selection(&mut self) {
        self.transcript_selection = None;
        self.selecting_transcript = false;
        self.transcript_dragged = false;
    }

    fn begin_transcript_selection(&mut self, column: u16, row: u16) {
        self.clear_transcript_selection();
        let Some(point) = selection_point(self.transcript_text_area, column, row, false) else {
            return;
        };
        self.transcript_selection = Some(TranscriptSelection {
            anchor: point,
            focus: point,
        });
        self.selecting_transcript = true;
    }

    fn update_transcript_selection(&mut self, column: u16, row: u16) {
        if !self.selecting_transcript {
            return;
        }
        let Some(point) = selection_point(self.transcript_text_area, column, row, true) else {
            return;
        };
        if let Some(selection) = &mut self.transcript_selection {
            selection.focus = point;
        }
    }

    fn drag_transcript_selection(&mut self, column: u16, row: u16) {
        self.transcript_dragged = true;
        self.update_transcript_selection(column, row);
    }

    fn finish_transcript_selection(&mut self, column: u16, row: u16) -> Option<String> {
        if !self.selecting_transcript {
            return None;
        }
        self.update_transcript_selection(column, row);
        self.selecting_transcript = false;
        let selection = self.transcript_selection?;
        if selection.anchor == selection.focus && !self.transcript_dragged {
            let target = self
                .transcript_hit_targets
                .get(selection.focus.row)
                .cloned()
                .flatten();
            self.transcript_selection = None;
            if let Some(target) = target {
                self.toggle_transcript_target(target);
            }
            return None;
        }
        let text = selected_transcript_text(&self.transcript_cells, selection);
        if text.is_none() {
            self.transcript_selection = None;
        }
        text
    }

    fn toggle_transcript_target(&mut self, target: TranscriptHitTarget) {
        match target {
            TranscriptHitTarget::ToolGroup(id) => {
                if !self.expanded_tool_groups.remove(&id) {
                    self.expanded_tool_groups.insert(id);
                }
            }
            TranscriptHitTarget::Tool(id) => {
                toggle_tool_expansion(&mut self.lines, &id);
            }
            TranscriptHitTarget::Thought(id) => {
                for line in &mut self.lines {
                    match line {
                        Line_::Thought {
                            id: line_id,
                            expanded,
                            ..
                        }
                        | Line_::Task {
                            id: line_id,
                            expanded,
                            ..
                        }
                        | Line_::Job {
                            id: line_id,
                            expanded,
                            ..
                        } if *line_id == id => {
                            *expanded = !*expanded;
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }
        self.transcript_cache.entries.clear();
        self.transcript_revision = self.transcript_revision.wrapping_add(1);
    }

    fn copy_to_clipboard(&mut self, text: &str) -> Result<()> {
        if self.clipboard.is_none() {
            self.clipboard = Some(arboard::Clipboard::new().context("opening clipboard")?);
        }
        self.clipboard
            .as_mut()
            .expect("clipboard initialized")
            .set_text(text.to_string())
            .context("writing clipboard")
    }

    #[cfg(test)]
    fn transcript_lines(&self, width: u16, now: std::time::Instant) -> Vec<Line<'static>> {
        use transcript::{transcript_entries, transcript_entry_rows};

        let mut output = Vec::new();
        for (index, entry) in transcript_entries(&self.lines, &self.expanded_tool_groups)
            .iter()
            .enumerate()
        {
            if index > 0 && !entry.is_child() {
                output.push(Line::default());
            }
            output.extend(
                transcript_entry_rows(
                    entry,
                    &self.expanded_tool_groups,
                    width,
                    now,
                    self.activity_started,
                    false,
                )
                .into_iter()
                .map(|row| row.line),
            );
        }
        if let Some(activity) = activity_line(
            self.busy,
            self.activity,
            now,
            self.activity_started,
            stream_liveness(
                self.stream_received,
                self.stream_last_data,
                self.stream_rate,
                now,
            )
            .as_deref(),
        ) {
            if !output.is_empty() {
                output.push(Line::default());
            }
            output.push(activity);
        }
        output
    }

    fn model_status_label(&self, include_provider: bool, width: usize) -> String {
        let model = if include_provider {
            self.current_model.as_str()
        } else {
            self.current_model
                .split_once('/')
                .map(|(_, model)| model)
                .unwrap_or(&self.current_model)
        };
        let Some(variant) = self.current_variant.as_deref() else {
            return truncate_display(model, width, Truncation::Right);
        };
        let suffix = format!("@{variant}");
        let suffix_width = UnicodeWidthStr::width(suffix.as_str());
        if suffix_width > width {
            return String::new();
        }
        let model = truncate_display(model, width.saturating_sub(suffix_width), Truncation::Right);
        format!("{model}{suffix}")
    }

    fn set_notice(&mut self, text: impl Into<String>, level: NoticeLevel) {
        self.set_notice_with_lifetime(text, level, level == NoticeLevel::Error);
    }

    fn set_persistent_notice(&mut self, text: impl Into<String>, level: NoticeLevel) {
        self.set_notice_with_lifetime(text, level, true);
    }

    fn set_notice_with_lifetime(
        &mut self,
        text: impl Into<String>,
        level: NoticeLevel,
        persistent: bool,
    ) {
        if self.notice.as_ref().is_some_and(|current| {
            current.persistent
                && (!persistent
                    || (current.level == NoticeLevel::Error && level != NoticeLevel::Error))
        }) {
            return;
        }
        let text = text.into();
        let text = text
            .lines()
            .next()
            .map(safe_text)
            .unwrap_or_default()
            .chars()
            .take(240)
            .collect::<String>();
        self.notice = (!text.is_empty()).then_some(StatusNotice {
            text,
            level,
            persistent,
        });
    }

    fn clear_notice(&mut self) {
        self.notice = None;
    }

    fn clear_transient_notice(&mut self) {
        if self
            .notice
            .as_ref()
            .is_some_and(|notice| !notice.persistent)
        {
            self.notice = None;
        }
    }

    fn operational_notice(&self) -> Option<(&str, Color)> {
        self.notice.as_ref().map(|notice| {
            let color = match notice.level {
                NoticeLevel::Info => theme::PRIMARY,
                NoticeLevel::Warning => theme::WAITING,
                NoticeLevel::Error => theme::ERROR,
            };
            (notice.text.as_str(), color)
        })
    }

    fn status_line(&self, width: u16) -> Line<'static> {
        let width = width as usize;
        if self.search_active {
            let counter = if self.search_matches.is_empty() {
                "no matches".to_string()
            } else {
                format!("{}/{}", self.search_current + 1, self.search_matches.len())
            };
            let hints = if width >= 64 {
                " · ↑↓ jump · ↵ keep · Esc back"
            } else {
                ""
            };
            return Line::from(vec![
                Span::styled(" /", Style::default().fg(theme::WAITING)),
                Span::raw(truncate_display(
                    &self.search_query,
                    width.saturating_sub(24).max(4),
                    Truncation::Middle,
                )),
                Span::styled("▏", Style::default().fg(theme::WAITING)),
                Span::styled(
                    truncate_display(
                        &format!(" {counter}{hints}"),
                        width.saturating_sub(4),
                        Truncation::Right,
                    ),
                    Style::default().fg(MUTED),
                ),
            ]);
        }
        let (icon, state, state_color) = match self.activity {
            Activity::Ready => ("●", "ready", theme::SUCCESS),
            Activity::Thinking => ("○", "thinking", theme::REASONING),
            Activity::Responding => ("▸", "responding", ASSISTANT),
            Activity::Tools => ("◆", "tools", TOOL_ACTIVE),
            Activity::Aborting => ("■", "aborting", theme::WAITING),
            Activity::Aborted => ("■", "aborted", theme::WAITING),
            Activity::Stopped => ("■", "stopped", theme::WAITING),
            Activity::Paused => ("Ⅱ", "paused", theme::WAITING),
            Activity::Error => ("×", "error", ERROR),
        };
        // Stream liveness: the spinner animates on wall-clock time, so
        // only arriving bytes prove the provider is not hanging.
        let state = match self.activity {
            Activity::Thinking | Activity::Responding if width >= 48 => stream_liveness(
                self.stream_received,
                self.stream_last_data,
                self.stream_rate,
                std::time::Instant::now(),
            )
            .map(|liveness| format!("{state} · {liveness}"))
            .unwrap_or_else(|| state.to_string()),
            _ => state.to_string(),
        };
        let state = state.as_str();
        let context = context_usage(
            self.context_used,
            self.context_limit,
            self.context_estimated,
        );
        let percent_color = match self
            .context_limit
            .filter(|limit| *limit > 0)
            .map(|limit| self.context_used.saturating_mul(100) / limit)
        {
            Some(percent) if percent >= 85 => ERROR,
            Some(percent) if percent >= 70 => theme::WAITING,
            _ => MUTED,
        };
        let percent = self
            .context_limit
            .filter(|limit| *limit > 0)
            .map(|limit| format!("{}%", self.context_used.saturating_mul(100) / limit))
            .unwrap_or_else(|| "—%".into());
        let meter = context_meter(
            self.context_used,
            self.context_limit,
            self.context_estimated,
            8,
        );
        if let Some((notice, notice_color)) = self.operational_notice() {
            let right = if width >= 80 {
                meter.unwrap_or_else(|| percent.clone())
            } else {
                percent.clone()
            };
            let right_width = UnicodeWidthStr::width(right.as_str());
            let prefix = format!(" {icon} ");
            let prefix_width = UnicodeWidthStr::width(prefix.as_str());
            if width <= right_width.saturating_add(prefix_width) {
                return Line::from(Span::styled(
                    truncate_display(
                        &format!("{prefix}{notice} {right}"),
                        width,
                        Truncation::Right,
                    ),
                    Style::default().fg(state_color),
                ));
            }
            let left_budget = width.saturating_sub(right_width).saturating_sub(1);
            let notice = truncate_display(
                notice,
                left_budget.saturating_sub(prefix_width),
                Truncation::Right,
            );
            let left_width = prefix_width + UnicodeWidthStr::width(notice.as_str());
            let gap = width.saturating_sub(left_width).saturating_sub(right_width);
            return Line::from(vec![
                Span::styled(prefix, Style::default().fg(state_color)),
                Span::styled(notice, Style::default().fg(notice_color)),
                Span::raw(" ".repeat(gap)),
                Span::styled(right, Style::default().fg(percent_color)),
            ]);
        }
        let context_display = if width >= 100 {
            meter.unwrap_or_else(|| context.clone())
        } else {
            context.clone()
        };
        // While a step streams, the provider hasn't reported usage yet —
        // tick the output figure live from streamed bytes (~4 bytes/token)
        // instead of showing the previous step's stale numbers.
        let live_out = (self.busy
            && matches!(self.activity, Activity::Thinking | Activity::Responding)
            && self.stream_received > self.stream_step_base)
            .then_some((self.stream_received - self.stream_step_base) / 4);
        let compact_latest_usage = match (self.latest_usage, live_out) {
            (Some(latest), Some(out)) => Some(format!(
                "i{}/o~{} req-cache r{}/w{} {percent}",
                format_tokens_compact(latest.input_tokens),
                format_tokens_compact(out),
                format_tokens_compact(latest.cache_read_input_tokens),
                format_tokens_compact(latest.cache_creation_input_tokens)
            )),
            (Some(latest), None) => Some(format!(
                "i{}/o{} req-cache r{}/w{} {percent}",
                format_tokens_compact(latest.input_tokens),
                format_tokens_compact(latest.output_tokens),
                format_tokens_compact(latest.cache_read_input_tokens),
                format_tokens_compact(latest.cache_creation_input_tokens)
            )),
            (None, Some(out)) => Some(format!("o~{} {percent}", format_tokens_compact(out))),
            (None, None) => None,
        };
        if width < 64 {
            let usage = compact_latest_usage
                .clone()
                .unwrap_or_else(|| match self.context_limit {
                    Some(limit) => format!(
                        "{}{}/{} {percent}",
                        if self.context_estimated { "~" } else { "" },
                        format_tokens_compact(self.context_used),
                        format_tokens_compact(limit)
                    ),
                    None => format!(
                        "{}{}/? {percent}",
                        if self.context_estimated { "~" } else { "" },
                        format_tokens_compact(self.context_used)
                    ),
                });
            let state_text = format!(" {icon} {state}");
            if width
                < UnicodeWidthStr::width(state_text.as_str())
                    + UnicodeWidthStr::width(usage.as_str())
                    + 5
            {
                return Line::from(Span::styled(
                    truncate_display(&format!("{state_text} {usage}"), width, Truncation::Right),
                    Style::default().fg(state_color),
                ));
            }
            let available = width
                .saturating_sub(UnicodeWidthStr::width(state_text.as_str()))
                .saturating_sub(UnicodeWidthStr::width(usage.as_str()))
                .saturating_sub(3);
            let model_budget = if self.latest_usage.is_some() {
                available
            } else {
                available.saturating_mul(3) / 5
            };
            let model = self.model_status_label(false, model_budget.max(1));
            let cwd = self.latest_usage.is_none().then(|| {
                let basename = self
                    .cwd
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("/");
                truncate_display(
                    basename,
                    available
                        .saturating_sub(UnicodeWidthStr::width(model.as_str()))
                        .max(1),
                    Truncation::Middle,
                )
            });
            let middle = match (model.is_empty(), cwd.as_deref()) {
                (true, Some(cwd)) => format!(" {cwd}"),
                (true, None) => String::new(),
                (false, Some(cwd)) => format!(" {model} {cwd}"),
                (false, None) => format!(" {model}"),
            };
            let used = UnicodeWidthStr::width(state_text.as_str())
                + UnicodeWidthStr::width(middle.as_str())
                + UnicodeWidthStr::width(usage.as_str())
                + 1;
            return Line::from(vec![
                Span::styled(state_text, Style::default().fg(state_color)),
                Span::styled(middle, Style::default().fg(MUTED)),
                Span::raw(" ".repeat(width.saturating_sub(used).max(1))),
                Span::styled(usage, Style::default().fg(percent_color)),
            ]);
        }
        let state_width = UnicodeWidthStr::width(state) + 3;
        let separators = 7;
        let session_total = {
            let total = self.session_usage;
            let tokens = total.input_tokens
                + total.output_tokens
                + total.cache_read_input_tokens
                + total.cache_creation_input_tokens;
            (tokens > 0).then(|| match self.session_cost {
                Some(cost) => {
                    format!("Σ {} {}", format_tokens_compact(tokens), format_cost(cost))
                }
                None if ilar::model::plan_billed(&self.current_model) => {
                    format!("Σ {} plan", format_tokens_compact(tokens))
                }
                None => format!("Σ {}", format_tokens_compact(tokens)),
            })
        };
        let detailed_usage = self.latest_usage.map(|latest| {
            let session = session_total
                .as_deref()
                .map(|total| format!("{total} · "))
                .unwrap_or_default();
            let out = match live_out {
                Some(out) => format!("~{out}"),
                None => latest.output_tokens.to_string(),
            };
            format!(
                "in {} · out {out} · req cache r{}/w{} · {session}{context_display}",
                latest.input_tokens,
                latest.cache_read_input_tokens,
                latest.cache_creation_input_tokens
            )
        });
        let detailed_usage = detailed_usage.filter(|usage| {
            UnicodeWidthStr::width(usage.as_str())
                .saturating_add(state_width)
                .saturating_add(separators)
                .saturating_add(20)
                <= width
        });
        let show_cwd = detailed_usage.is_some() || compact_latest_usage.is_none();
        let usage = detailed_usage
            .or(compact_latest_usage)
            .unwrap_or(context_display);
        let usage = truncate_display(
            &usage,
            width.saturating_sub(state_width + separators + 8),
            Truncation::Right,
        );
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let cwd = show_cwd.then(|| abbreviated_path(&self.cwd, home.as_deref()));
        let usage_width = UnicodeWidthStr::width(usage.as_str());
        let available = width
            .saturating_sub(state_width)
            .saturating_sub(usage_width)
            .saturating_sub(separators);
        let model_budget = if !show_cwd {
            available
        } else if width >= 80 {
            available.saturating_mul(3) / 5
        } else {
            available / 2
        };
        let model = self.model_status_label(width >= 80, model_budget.max(4));
        let cwd = cwd.map(|cwd| {
            truncate_display(
                &cwd,
                available
                    .saturating_sub(UnicodeWidthStr::width(model.as_str()))
                    .max(4),
                Truncation::Middle,
            )
        });
        let detail = match (model.is_empty(), cwd.as_deref()) {
            (true, Some(cwd)) => format!(" · {cwd}"),
            (true, None) => String::new(),
            (false, Some(cwd)) => format!(" · {model} · {cwd}"),
            (false, None) => format!(" · {model}"),
        };
        let left = format!(" {icon} {state}{detail}");
        let gap = width
            .saturating_sub(UnicodeWidthStr::width(left.as_str()))
            .saturating_sub(usage_width)
            .max(1);
        Line::from(vec![
            Span::styled(format!(" {icon} {state}"), Style::default().fg(state_color)),
            Span::styled(detail, Style::default().fg(MUTED)),
            Span::raw(" ".repeat(gap)),
            Span::styled(usage, Style::default().fg(percent_color)),
        ])
    }

    fn render(&mut self, frame: &mut Frame) {
        let desired_input_height = self.input.line_count().min(6) as u16 + 2;
        let input_height = desired_input_height.min(frame.area().height.saturating_sub(4).max(3));
        let chunks = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(input_height),
        ])
        .split(frame.area());

        let content_areas = content_areas(chunks[0]);
        let transcript_area = content_areas.transcript;
        let text_width = transcript_area
            .width
            .saturating_sub(2 + CONTENT_HORIZONTAL_PADDING * 2);
        let now = std::time::Instant::now();
        self.transcript_cache.update(
            &self.lines,
            &self.expanded_tool_groups,
            self.transcript_revision,
            text_width,
            now,
            self.activity_started,
        );
        // Streaming shifts row indices; keep search matches in sync with
        // the rows actually on screen.
        if self.search_active && self.search_computed_revision != Some(self.transcript_revision) {
            self.search_matches = self.transcript_cache.matching_rows(&self.search_query);
            self.search_current = self
                .search_current
                .min(self.search_matches.len().saturating_sub(1));
            self.search_computed_revision = Some(self.transcript_revision);
        }
        let mut activity_rows = activity_line(
            self.busy,
            self.activity,
            now,
            self.activity_started,
            stream_liveness(
                self.stream_received,
                self.stream_last_data,
                self.stream_rate,
                now,
            )
            .as_deref(),
        )
        .into_iter()
        .flat_map(|line| wrap_styled_line(line, text_width as usize))
        .collect::<Vec<_>>();
        if !activity_rows.is_empty() && self.transcript_cache.row_count() > 0 {
            activity_rows.insert(0, Line::default());
        }
        let viewport_rows = transcript_area.height.saturating_sub(2) as usize;
        let content_rows = self
            .transcript_cache
            .row_count()
            .saturating_add(activity_rows.len());
        self.update_scroll_metrics(content_rows, viewport_rows);
        let visible_rows = content_rows
            .saturating_sub(self.scroll_top)
            .min(viewport_rows) as u16;
        let transcript_text_area = Rect::new(
            transcript_area
                .x
                .saturating_add(1 + CONTENT_HORIZONTAL_PADDING),
            transcript_area.y.saturating_add(1),
            text_width,
            visible_rows,
        );
        let max_scroll = self.max_scroll();
        let scroll_label = if max_scroll == 0 {
            String::new()
        } else if self.follow_tail {
            " · tail".into()
        } else {
            format!(" · {}%", self.scroll_top.saturating_mul(100) / max_scroll)
        };
        let mut transcript_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Plain)
            .border_style(theme::panel_border())
            .title(Line::from(vec![
                Span::styled("ilar", theme::title(theme::ASSISTANT)),
                Span::styled(scroll_label, Style::default().fg(theme::MUTED)),
            ]))
            .padding(Padding::new(
                CONTENT_HORIZONTAL_PADDING,
                CONTENT_HORIZONTAL_PADDING,
                0,
                0,
            ));
        if content_areas.todos.is_none() && transcript_area.height > 1 {
            let snapshot = {
                let todos = self.todos.lock().unwrap();
                todo_render_snapshot(&todos, 1)
            };
            if let Some(summary) = todo_summary(&snapshot, transcript_area.width.saturating_sub(4))
            {
                transcript_block = transcript_block.title_bottom(summary.right_aligned());
            }
        }
        let visible =
            self.transcript_cache
                .visible_rows(self.scroll_top, viewport_rows, &activity_rows);
        self.transcript_hit_targets = visible.iter().map(|row| row.target.clone()).collect();
        let text = visible
            .into_iter()
            .enumerate()
            .map(|(offset, row)| {
                let mut line = row.line;
                if self.search_active
                    && !self.search_query.is_empty()
                    && self
                        .search_matches
                        .binary_search(&(self.scroll_top + offset))
                        .is_ok()
                {
                    let current = self.search_matches.get(self.search_current)
                        == Some(&(self.scroll_top + offset));
                    for span in &mut line.spans {
                        span.style = span.style.add_modifier(if current {
                            Modifier::REVERSED | Modifier::BOLD
                        } else {
                            Modifier::REVERSED
                        });
                    }
                }
                line
            })
            .collect::<Vec<_>>();
        let paragraph = Paragraph::new(text).block(transcript_block);
        frame.render_widget(paragraph, transcript_area);

        if max_scroll > 0 && transcript_area.height > 2 {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("│"))
                .thumb_symbol("┃");
            let mut state = ScrollbarState::new(content_rows)
                .position(self.scroll_top)
                .viewport_content_length(self.viewport_rows);
            let area = Rect::new(
                transcript_area.right().saturating_sub(2),
                transcript_area.y.saturating_add(1),
                1,
                transcript_area.height.saturating_sub(2),
            );
            frame.render_stateful_widget(scrollbar, area, &mut state);
        }

        if let Some(todo_area) = content_areas.todos {
            let mut todo_area = todo_area;
            if let Some((goal, round)) = &self.goal {
                let text_width = todo_area
                    .width
                    .saturating_sub(2 + CONTENT_HORIZONTAL_PADDING * 2)
                    .max(1) as usize;
                let mut lines: Vec<Line<'static>> = safe_lines(goal)
                    .into_iter()
                    .flat_map(|line| wrap_styled_line(Line::raw(line), text_width))
                    .take(5)
                    .map(|mut line| {
                        for span in &mut line.spans {
                            span.style = span.style.fg(theme::PRIMARY);
                        }
                        line
                    })
                    .collect();
                lines.push(Line::styled(
                    truncate_display(
                        "Ctrl-Q manage · /goal edit · /goal abort",
                        text_width,
                        Truncation::Right,
                    ),
                    Style::default().fg(MUTED),
                ));
                let height = (lines.len() as u16 + 2).min(todo_area.height / 2);
                if height > 2 {
                    let goal_area = Rect::new(todo_area.x, todo_area.y, todo_area.width, height);
                    todo_area = Rect::new(
                        todo_area.x,
                        todo_area.y + height,
                        todo_area.width,
                        todo_area.height - height,
                    );
                    let goal_block = Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(theme::panel_border())
                        .padding(Padding::new(
                            CONTENT_HORIZONTAL_PADDING,
                            CONTENT_HORIZONTAL_PADDING,
                            0,
                            0,
                        ))
                        .title(Line::from(Span::styled(
                            format!("goal {round}/{MAX_GOAL_ROUNDS}"),
                            theme::title(theme::REASONING),
                        )));
                    frame.render_widget(Paragraph::new(lines).block(goal_block), goal_area);
                }
            }
            if !self.services_view.is_empty() {
                let text_width = todo_area
                    .width
                    .saturating_sub(2 + CONTENT_HORIZONTAL_PADDING * 2)
                    .max(1) as usize;
                let shown = self.services_view.len().min(4);
                let mut lines: Vec<Line<'static>> = self
                    .services_view
                    .iter()
                    .take(shown)
                    .map(|(name, running, detail)| {
                        let (marker, color) = if *running {
                            ("● ", theme::SUCCESS)
                        } else {
                            ("○ ", MUTED)
                        };
                        Line::from(vec![
                            Span::styled(marker, Style::default().fg(color)),
                            Span::styled(
                                truncate_display(
                                    &format!("{name} · {detail}"),
                                    text_width.saturating_sub(2),
                                    Truncation::Right,
                                ),
                                Style::default().fg(if *running { theme::PRIMARY } else { MUTED }),
                            ),
                        ])
                    })
                    .collect();
                if self.services_view.len() > shown {
                    lines.push(Line::styled(
                        format!("  +{} more", self.services_view.len() - shown),
                        Style::default().fg(MUTED),
                    ));
                }
                let height = (lines.len() as u16 + 2).min(todo_area.height / 2);
                if height > 2 {
                    let service_area = Rect::new(todo_area.x, todo_area.y, todo_area.width, height);
                    todo_area = Rect::new(
                        todo_area.x,
                        todo_area.y + height,
                        todo_area.width,
                        todo_area.height - height,
                    );
                    let service_block = Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(theme::panel_border())
                        .padding(Padding::new(
                            CONTENT_HORIZONTAL_PADDING,
                            CONTENT_HORIZONTAL_PADDING,
                            0,
                            0,
                        ))
                        .title(Line::from(Span::styled(
                            format!("services ({})", self.services_running),
                            theme::title(TOOL_ACTIVE),
                        )));
                    frame.render_widget(Paragraph::new(lines).block(service_block), service_area);
                }
            }
            let todo_block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(theme::panel_border())
                .padding(Padding::new(
                    CONTENT_HORIZONTAL_PADDING,
                    CONTENT_HORIZONTAL_PADDING,
                    0,
                    0,
                ))
                .title(Line::from(Span::styled(
                    "todos",
                    theme::title(theme::WAITING),
                )));
            let inner = todo_block.inner(todo_area);
            let snapshot = {
                let todos = self.todos.lock().unwrap();
                todo_sidebar_snapshot(&todos, inner.height as usize)
            };
            let lines = render_todo_sidebar_snapshot(&snapshot, inner.width, inner.height);
            frame.render_widget(Paragraph::new(lines).block(todo_block), todo_area);
        }

        frame.render_widget(Paragraph::new(self.status_line(chunks[1].width)), chunks[1]);

        let input_focused = input_accepts_keys(self.busy, self.has_modal());
        let input_block = Block::default()
            .borders(Borders::ALL)
            .border_type(if input_focused {
                BorderType::Thick
            } else {
                BorderType::Plain
            })
            .border_style(if input_focused {
                theme::focus_border()
            } else {
                theme::panel_border()
            });
        let input_area = input_block.inner(chunks[2]);
        let input_view = self
            .input
            .multiline_view(input_area.width, input_area.height);
        let mut input_title = if input_view.line_count > 1 {
            format!(
                " input {}/{} ",
                input_view.cursor_line, input_view.line_count
            )
        } else {
            " input ".into()
        };
        if !self.queued_messages.is_empty() {
            input_title = format!("{}· {} queued ", input_title, self.queued_messages.len());
        }
        if let Some((_, round)) = &self.goal {
            input_title = format!("{input_title}· goal {round}/{MAX_GOAL_ROUNDS} ");
        }
        let input_lines = input_view
            .lines
            .iter()
            .cloned()
            .map(Line::raw)
            .collect::<Vec<_>>();
        let mut input_block = input_block.title(Line::styled(
            input_title,
            theme::title(if input_focused {
                theme::USER
            } else {
                theme::SECONDARY
            }),
        ));
        let input_help = if chunks[2].width >= 48 {
            " Enter send · Shift-Enter/Ctrl-J newline "
        } else {
            " Enter send "
        };
        input_block = input_block.title_bottom(
            Line::styled(input_help, Style::default().fg(theme::MUTED)).right_aligned(),
        );
        let input = Paragraph::new(input_lines)
            .style(Style::default().fg(theme::PRIMARY))
            .block(input_block);
        frame.render_widget(input, chunks[2]);

        // Inline slash-completion popup anchored above the input.
        let candidates = slash_candidates(self.input.text(), &self.skills);
        if !candidates.is_empty() && !self.has_modal() {
            let rows = candidates.len().min(6) as u16;
            let height = rows + 2;
            let width = chunks[2].width.clamp(20, 64);
            let popup = Rect::new(
                chunks[2].x,
                chunks[2].y.saturating_sub(height),
                width,
                height.min(chunks[2].y),
            );
            if popup.height > 2 {
                frame.render_widget(Clear, popup);
                let block = Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme::focus_border())
                    .title(Line::styled(" commands ", theme::title(theme::MARKUP)))
                    .title_bottom(
                        Line::styled(" ↑↓ · Tab/↵ complete ", Style::default().fg(theme::MUTED))
                            .right_aligned(),
                    );
                let inner = block.inner(popup);
                frame.render_widget(block, popup);
                let selected = self.slash_selected.min(candidates.len() - 1);
                let lines: Vec<Line<'static>> = candidates
                    .iter()
                    .enumerate()
                    .skip(selected.saturating_sub(inner.height as usize - 1))
                    .take(inner.height as usize)
                    .map(|(index, (name, description))| {
                        let marker = if index == selected { "> " } else { "  " };
                        let text = truncate_display(
                            &format!("{marker}/{name} — {description}"),
                            inner.width as usize,
                            Truncation::Right,
                        );
                        let style = if index == selected {
                            theme::selected()
                        } else {
                            Style::default().fg(theme::PRIMARY)
                        };
                        Line::styled(
                            format!("{text:<width$}", width = inner.width as usize),
                            style,
                        )
                    })
                    .collect();
                frame.render_widget(Paragraph::new(lines), inner);
            }
        }

        let transcript_cells = transcript_cells(frame.buffer_mut(), transcript_text_area);
        if self.transcript_selection.is_some_and(|selection| {
            self.transcript_text_area != transcript_text_area
                || !selected_rows_unchanged(&self.transcript_cells, &transcript_cells, selection)
        }) {
            self.clear_transcript_selection();
        }
        self.transcript_text_area = transcript_text_area;
        self.transcript_cells = transcript_cells;
        if let Some(selection) = self.transcript_selection {
            highlight_transcript_selection(
                frame.buffer_mut(),
                self.transcript_text_area,
                selection,
                &self.transcript_cells,
            );
        }

        if input_accepts_keys(self.busy, self.has_modal())
            && input_area.width > 0
            && input_area.height > 0
        {
            frame.set_cursor_position((
                input_area.x.saturating_add(input_view.cursor_x),
                input_area.y.saturating_add(input_view.cursor_y),
            ));
        }

        // Same precedence the key dispatcher uses, from the same value:
        // whatever is drawn on top is whatever is taking the keys.
        match self.active_modal() {
            Some(Modal::PendingManager) => render_pending_manager(frame, self),
            Some(Modal::Help) => render_help(frame, self.help_scroll, self.keyboard_enhanced),
            Some(Modal::ThemePicker) => {
                render_theme_picker(frame, self.theme_picker.as_ref().expect("theme picker"));
            }
            Some(Modal::SkillPicker) => {
                render_skill_picker(frame, self.skill_picker.as_ref().expect("skill picker"));
            }
            Some(Modal::SessionPicker) => {
                render_session_picker(frame, self.session_picker.as_ref().expect("session picker"));
            }
            Some(Modal::ModelPicker) => {
                render_model_picker(frame, self.model_picker.as_ref().expect("model picker"));
            }
            Some(Modal::VariantPicker) => {
                render_variant_picker(frame, self.variant_picker.as_ref().expect("variant picker"));
            }
            // Search renders into the status line, not an overlay.
            Some(Modal::Search) => {}
            Some(Modal::CommandPalette) => {
                render_command_palette(frame, self.command_palette.as_ref().expect("palette"));
            }
            None => {}
        }
        theme::apply(frame.buffer_mut(), self.theme);
    }
}

struct TodoRenderSnapshot {
    items: Vec<ilar::todo::TodoItem>,
    hidden: usize,
}

fn todo_render_snapshot(list: &ilar::todo::TodoList, cap: usize) -> TodoRenderSnapshot {
    let indices = visible_todo_indices(list, cap);
    TodoRenderSnapshot {
        hidden: list.items.len().saturating_sub(indices.len()),
        items: indices
            .into_iter()
            .map(|index| list.items[index].clone())
            .collect(),
    }
}

fn todo_sidebar_snapshot(list: &ilar::todo::TodoList, height: usize) -> TodoRenderSnapshot {
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

fn render_todo_sidebar_snapshot(
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

fn todo_summary(snapshot: &TodoRenderSnapshot, width: u16) -> Option<Line<'static>> {
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

fn selection_point(area: Rect, column: u16, row: u16, clamp: bool) -> Option<SelectionPoint> {
    if area.width == 0 || area.height == 0 {
        return None;
    }
    if !clamp && (column < area.x || column >= area.right() || row < area.y || row >= area.bottom())
    {
        return None;
    }
    let column = column.clamp(area.x, area.right().saturating_sub(1));
    let row = row.clamp(area.y, area.bottom().saturating_sub(1));
    Some(SelectionPoint {
        row: row.saturating_sub(area.y) as usize,
        column: column.saturating_sub(area.x) as usize,
    })
}

fn selected_columns(
    selection: TranscriptSelection,
    row: usize,
    width: usize,
) -> Option<std::ops::RangeInclusive<usize>> {
    if width == 0 || selection.anchor == selection.focus {
        return None;
    }
    let (start, end) = selection.ordered();
    if row < start.row || row > end.row {
        return None;
    }
    let first = if row == start.row { start.column } else { 0 }.min(width - 1);
    let last = if row == end.row {
        end.column
    } else {
        width - 1
    }
    .min(width - 1);
    (first <= last).then_some(first..=last)
}

fn grapheme_columns(
    row: &RenderedRow,
    columns: std::ops::RangeInclusive<usize>,
) -> std::ops::RangeInclusive<usize> {
    let mut first = *columns.start();
    let mut last = *columns.end();
    if let Some(RenderedCell::Continuation { lead }) = row.get(first) {
        first = *lead;
    }
    if let Some(RenderedCell::Continuation { lead }) = row.get(last) {
        last = *lead;
    }
    while matches!(row.get(last + 1), Some(RenderedCell::Continuation { lead }) if *lead == last) {
        last += 1;
    }
    first..=last
}

fn selected_transcript_text(
    rows: &[RenderedRow],
    selection: TranscriptSelection,
) -> Option<String> {
    if selection.anchor == selection.focus || rows.is_empty() {
        return None;
    }
    let (start, end) = selection.ordered();
    let last_row = end.row.min(rows.len().saturating_sub(1));
    if start.row > last_row {
        return None;
    }
    let selected = (start.row..=last_row)
        .map(|row| {
            let cells = &rows[row];
            selected_columns(selection, row, cells.len())
                .map(|columns| {
                    let mut text = String::new();
                    for column in grapheme_columns(cells, columns) {
                        match cells.get(column) {
                            Some(RenderedCell::Character(value)) => text.push(*value),
                            Some(RenderedCell::Text(value)) => text.push_str(value),
                            Some(RenderedCell::Space) => text.push(' '),
                            Some(RenderedCell::Continuation { .. }) | None => {}
                        }
                    }
                    text.trim_end_matches(' ').to_string()
                })
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!selected.is_empty()).then_some(selected)
}

fn selected_rows_unchanged(
    previous: &[RenderedRow],
    current: &[RenderedRow],
    selection: TranscriptSelection,
) -> bool {
    let (start, end) = selection.ordered();
    (start.row..=end.row).all(|row| previous.get(row) == current.get(row))
}

fn transcript_cells(buffer: &Buffer, area: Rect) -> Vec<RenderedRow> {
    (area.y..area.bottom())
        .map(|row| {
            let mut rendered = Vec::with_capacity(area.width as usize);
            let mut column = area.x;
            while column < area.right() {
                let symbol = buffer
                    .cell((column, row))
                    .map(|cell| cell.symbol())
                    .unwrap_or(" ");
                if symbol == " " {
                    rendered.push(RenderedCell::Space);
                    column += 1;
                    continue;
                }
                let lead = rendered.len();
                let width = UnicodeWidthStr::width(symbol)
                    .max(1)
                    .min(area.right().saturating_sub(column) as usize);
                let mut characters = symbol.chars();
                match (characters.next(), characters.next()) {
                    (Some(character), None) => {
                        rendered.push(RenderedCell::Character(character));
                    }
                    _ => rendered.push(RenderedCell::Text(symbol.to_string())),
                }
                for _ in 1..width {
                    rendered.push(RenderedCell::Continuation { lead });
                }
                column = column.saturating_add(width as u16);
            }
            rendered
        })
        .collect()
}

fn highlight_transcript_selection(
    buffer: &mut Buffer,
    area: Rect,
    selection: TranscriptSelection,
    rows: &[RenderedRow],
) {
    for row in 0..area.height as usize {
        let Some(rendered) = rows.get(row) else {
            continue;
        };
        let Some(columns) = selected_columns(selection, row, rendered.len()) else {
            continue;
        };
        for column in grapheme_columns(rendered, columns) {
            if let Some(cell) = buffer.cell_mut((
                area.x.saturating_add(column as u16),
                area.y.saturating_add(row as u16),
            )) {
                cell.set_style(Style::default().add_modifier(Modifier::REVERSED));
            }
        }
    }
}

/// Retry is a modifier chord, never a bare letter: the prompt has focus,
/// so any printable key must reach the input buffer.
fn retry_requested(code: KeyCode, control: bool) -> bool {
    control && matches!(code, KeyCode::Char('r' | 'R'))
}

/// Load the last prompt and return the synthetic Enter that resubmits it
/// through the ordinary path.
///
/// Declines when the input holds a draft: overwriting it would discard
/// text the user typed but never submitted, and an unsubmitted draft is
/// not in the history, so it would be unrecoverable.
fn begin_retry(app: &mut App) -> Option<Event> {
    if !app.input.is_blank() {
        app.set_notice(
            "input has an unsent draft — send or clear it before retrying",
            NoticeLevel::Warning,
        );
        return None;
    }
    let prompt = app.last_prompt.clone()?;
    app.retry_available = false;
    app.clear_notice();
    app.input = InputBuffer::from(prompt);
    Some(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )))
}

/// Whether keystrokes reach the input buffer, and so whether the caret
/// and focused border should be shown. Typing during a turn is allowed —
/// it queues — so `busy` must not hide the caret.
fn input_accepts_keys(_busy: bool, has_modal: bool) -> bool {
    !has_modal
}

/// Inline completion candidates for a slash input: built-in commands
/// plus skills, fuzzy-ranked. Empty once the name is finished (whitespace)
/// or the input is not a slash command.
fn slash_candidates(input: &str, skills: &[(String, String)]) -> Vec<(String, String)> {
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
    candidates.extend(skills.iter().cloned());
    let mut scored: Vec<(i64, (String, String))> = candidates
        .into_iter()
        .filter_map(|(name, description)| {
            fuzzy_score(token, &name).map(|score| (score, (name, description)))
        })
        .collect();
    scored.sort_by(|(score_a, (name_a, _)), (score_b, (name_b, _))| {
        score_b.cmp(score_a).then_with(|| name_a.cmp(name_b))
    });
    scored.into_iter().map(|(_, candidate)| candidate).collect()
}

/// Rounds after which goal mode gives up (a budget, unlike the
/// runaway-loop iteration guard).
const MAX_GOAL_ROUNDS: u32 = 25;
const GOAL_SENTINEL: &str = "GOAL_ACHIEVED";

/// True when the assistant's final text declares the goal achieved
/// (sentinel at a line start, so prose mentions don't trigger it).
fn goal_achieved_in(text: &str) -> bool {
    text.lines()
        .any(|line| line.trim_start().starts_with(GOAL_SENTINEL))
}

fn goal_kickoff_prompt(goal: &str) -> String {
    format!(
        "Work toward this goal: {goal}

This is a goal-mode session: after          each of your turns you will be asked to verify progress with          concrete evidence. If no automatic verification exists yet          (tests, a replay harness, a checker script), building one is part          of the goal. Do not claim success without evidence."
    )
}

fn goal_continuation_prompt(goal: &str, round: u32) -> String {
    format!(
        "Goal check, round {round}/{MAX_GOAL_ROUNDS}. The goal: {goal}

         Verify the current state with concrete evidence by running your          verification (tests, harness, checker) now — do not judge from          memory. If the goal is genuinely achieved, output a line starting          with `{GOAL_SENTINEL}:` followed by the evidence. Otherwise state          what is still missing and continue working toward the goal in          this same turn."
    )
}

/// `/name args` parsed from a submitted prompt; `None` when the text is
/// not shaped like a skill invocation (and should submit unchanged).
fn parse_slash_invocation(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix('/')?;
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    if name.is_empty()
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_".contains(character))
    {
        return None;
    }
    Some((name, args.trim()))
}

fn skill_invocation_prompt(name: &str, args: &str) -> String {
    if args.is_empty() {
        format!("Use the `skill` tool to load the skill \"{name}\" and follow its instructions.")
    } else {
        format!(
            "Use the `skill` tool to load the skill \"{name}\" and follow its instructions. Arguments: {args}"
        )
    }
}

fn close_skill_matches(skills: &[(String, String)], name: &str) -> Vec<String> {
    let lowered = name.to_lowercase();
    let mut matches: Vec<String> = skills
        .iter()
        .filter(|(candidate, _)| candidate.to_lowercase().contains(&lowered))
        .map(|(candidate, _)| candidate.clone())
        .collect();
    if matches.is_empty() {
        matches = skills
            .iter()
            .map(|(candidate, _)| candidate.clone())
            .collect();
    }
    matches.truncate(6);
    matches
}

fn selected_agent_name(cli: Option<&str>, persisted: Option<&str>) -> String {
    cli.or(persisted).unwrap_or("build").to_string()
}

fn selected_model(
    cli: Option<&str>,
    persisted: Option<&str>,
    agent: Option<&str>,
    general: &str,
) -> String {
    cli.or(persisted).or(agent).unwrap_or(general).to_string()
}

fn persist_model_change(
    resolver: &dyn ProviderResolver,
    store: &SessionStore,
    session_id: &str,
    model: &str,
    variant: Option<&str>,
) -> Result<()> {
    drop(resolver.resolve_provider(model)?);
    ilar::model::variant_options(model, variant)?;
    let mut session = store.acquire_writer(session_id)?.load()?;
    session.append(ilar::session::SessionEvent::ModelChange {
        id: ilar::session::new_id(),
        model: model.to_string(),
        variant: variant.map(String::from),
        ts: chrono::Utc::now(),
    })?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn adopt_model_selection(
    app: &mut App,
    resolver: &dyn ProviderResolver,
    store: &SessionStore,
    session_id: &str,
    system_prompt: &str,
    registry: &ToolRegistry,
    model: String,
    variant: Option<String>,
) -> Result<()> {
    persist_model_change(resolver, store, session_id, &model, variant.as_deref())?;
    app.current_model = model.clone();
    app.current_variant = variant.clone();
    app.context_limit = display_context_limit(resolver, &model);
    if let Ok((used, estimated)) =
        session_context_tokens(store, session_id, system_prompt, registry)
    {
        app.context_used = used;
        app.context_estimated = estimated;
    }
    app.status = "ready".into();
    app.clear_notice();
    let selection = variant
        .as_deref()
        .map(|variant| format!("{model}@{variant}"))
        .unwrap_or(model);
    app.push_transcript_line(Line_::System(format!("switched to {selection}")));
    Ok(())
}

fn ensure_direct_resume_allowed(meta: Option<&SessionMeta>) -> Result<()> {
    if meta.is_some_and(|meta| meta.workspace.is_some()) {
        anyhow::bail!("workspace-bound child sessions must be resumed through Task");
    }
    Ok(())
}

fn restored_todos(resumed: Option<&ilar::session::SessionReader>) -> ilar::todo::TodoList {
    resumed
        .and_then(ilar::session::SessionReader::todo_list)
        .cloned()
        .unwrap_or_default()
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let config = Loader::new().resolve().context("loading config")?;
    if let Some(Command::Login) = args.command {
        let store = ilar::auth::AuthStore::open(config.state_dir().to_path_buf());
        let tokens = ilar::auth::login_flow(&store, std::time::Duration::from_secs(300), true)
            .await
            .context("login failed")?;
        println!(
            "Logged in as ChatGPT account {}",
            tokens
                .account_id
                .as_deref()
                .unwrap_or("(account id unknown)")
        );
        println!("Tokens stored at {}", store.tokens_path().display());
        return Ok(());
    }

    let configured_theme = theme::ThemeId::parse(&config.general.theme).with_context(|| {
        format!(
            "unknown theme {:?}; expected one of: {}",
            config.general.theme,
            theme::ThemeId::ALL
                .iter()
                .map(|theme| theme.id())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;

    let store = SessionStore::new(config.state_dir().join("sessions"));
    // The whole runtime (agent, model, prompt, registry) is rebuilt per
    // session so switching via the picker restarts with full fidelity.
    let mut session_override: Option<String> = None;
    let mut first_run = true;
    let mut terminal_hold: Option<(ratatui::DefaultTerminal, TerminalSession)> = None;
    let mut active_theme = configured_theme;

    loop {
        let resume_target = if first_run {
            if args.continue_last {
                Some(
                    store
                        .latest()
                        .map(|session| session.id)
                        .context("no sessions to continue (session directory is empty)")?,
                )
            } else {
                args.session.clone()
            }
        } else {
            session_override.clone()
        };
        // CLI overrides apply to the launch session only, not picker switches.
        let cli_model = if first_run {
            args.model.as_deref()
        } else {
            None
        };
        let cli_agent = if first_run {
            args.agent.as_deref()
        } else {
            None
        };
        first_run = false;

        let resumed = resume_target
            .as_deref()
            .map(|id| {
                store
                    .load(id)
                    .with_context(|| format!("resuming session {id}"))
            })
            .transpose()?;
        ensure_direct_resume_allowed(resumed.as_ref().and_then(|session| session.meta()))?;
        let persisted_agent = resumed
            .as_ref()
            .and_then(|session| session.meta())
            .map(|meta| meta.agent.clone());
        let agent_name = selected_agent_name(cli_agent, persisted_agent.as_deref());
        let agents = config.agents().context("loading agent definitions")?;
        let agent = agents
            .iter()
            .find(|a| a.name == agent_name)
            .cloned()
            .with_context(|| format!("unknown agent {agent_name:?}"))?;
        let persisted_model = resumed.as_ref().map(|session| session.effective_model());
        let persisted_variant = resumed
            .as_ref()
            .and_then(|session| session.effective_variant());
        let model_for_session = selected_model(
            cli_model,
            persisted_model.as_deref(),
            agent.model.as_deref(),
            &config.general.model,
        );

        let cwd = std::env::current_dir().context("no cwd")?;
        let skill_store = std::sync::Arc::new(ilar::skill::SkillStore::new(
            config.dirs().0.to_path_buf(),
            config.dirs().1.to_path_buf(),
        ));
        let skill_listing = skill_store
            .listing_prompt()
            .context("loading skill definitions")?;
        let skill_inventory: Vec<(String, String)> = skill_store
            .list()
            .context("loading skill definitions")?
            .into_iter()
            .map(|skill| (skill.name, skill.description))
            .collect();
        let mut system_prompt =
            system_prompt_for(config.dirs().0, &cwd).context("loading project instructions")?;
        if !skill_listing.is_empty() {
            system_prompt = format!("{system_prompt}\n\n{skill_listing}");
        }
        if !agent.prompt.is_empty() {
            system_prompt = format!(
                "{system_prompt}\n\n# Agent: {}\n\n{}",
                agent.name, agent.prompt
            );
        }
        if args.print_prompt {
            println!("{system_prompt}");
            return Ok(());
        }

        let resolver: Arc<dyn ProviderResolver> = Arc::new(config.clone());
        drop(
        resolver
            .resolve_provider(&model_for_session)
            .with_context(|| format!("no provider configured for {model_for_session} (set ILAR_ZAI_API_KEY / ILAR_OPENAI_API_KEY)"))?,
    );

        let session_id = match &resume_target {
            Some(id) => {
                if cli_model.is_some()
                    && persisted_model.as_deref() != Some(model_for_session.as_str())
                {
                    persist_model_change(resolver.as_ref(), &store, id, &model_for_session, None)
                        .with_context(|| format!("persisting model override {model_for_session}"))?;
                }
                id.clone()
            }
            None => {
                let id = new_id();
                store
                    .create(SessionMeta {
                        session_id: id.clone(),
                        parent_id: None,
                        agent: agent.name.clone(),
                        model: model_for_session.clone(),
                        workspace: None,
                    })
                    .context("creating session")?;
                id
            }
        };

        let loop_config = LoopConfig {
            compaction_threshold: config.compaction.threshold,
            max_iterations: config.agent.max_iterations,
            ..LoopConfig::default()
        };
        let services = ilar::tools::service::ServiceManager::new();
        let spawner = std::sync::Arc::new(
            SubagentSpawner::new(
                resolver.clone(),
                store.clone(),
                agents,
                cwd.clone(),
                0,
                config.subagents.max_concurrent,
                config.subagents.max_depth,
            )
            .with_user_config_dir(config.dirs().0.to_path_buf())
            .with_background_tool_timeout(std::time::Duration::from_millis(
                config.subagents.background_tool_timeout_ms,
            ))
            .with_loop_config(loop_config.clone())
            .with_services(services.clone())
            .with_available_models(config.available_models()),
        );
        let todos = std::sync::Arc::new(std::sync::Mutex::new(restored_todos(resumed.as_ref())));
        let registry = ToolRegistry::builtin()
            .with_subagents(spawner.clone())?
            .with_services(services.clone())?
            .with_models(config.available_models())?
            .with_todos(todos.clone())?
            .with_web_tools()?
            .with_skills(skill_store)?;
        let notifications = spawner.subscribe();
        let subagent_activity = spawner.subscribe_activity();
        let tool_ctx = ToolContext::root(cwd.clone()).with_subagents(spawner.clone());
        let model_choices = config.available_models();
        let user_config_path = config.dirs().0.join("ilar.toml");

        let (context_used, context_estimated) =
            session_context_tokens(&store, &session_id, &system_prompt, &registry)?;
        let context_limit = display_context_limit(resolver.as_ref(), &model_for_session);
        let mut app = App::new();
        app.theme = active_theme;
        app.history = history::PromptHistory::load(config.state_dir().join("prompt_history.jsonl"));
        app.skills = skill_inventory;
        app.session_id = session_id.clone();
        app.todos = todos;
        if let Some(resumed) = &resumed {
            app.restore_session(resumed, &store);
        }
        app.configure_runtime(
            model_for_session.clone(),
            (persisted_model.as_deref() == Some(model_for_session.as_str()))
                .then_some(persisted_variant)
                .flatten(),
            cwd.clone(),
            context_used,
            context_limit,
            context_estimated,
        );

        if terminal_hold.is_none() {
            terminal_hold = Some(TerminalSession::start()?);
        }
        app.keyboard_enhanced = terminal_hold
            .as_ref()
            .is_some_and(|(_, session)| session.keyboard_enhanced);
        let terminal = &mut terminal_hold.as_mut().expect("terminal started").0;
        let exit = run_app(
            terminal,
            &mut app,
            &user_config_path,
            resolver,
            &store,
            &session_id,
            &system_prompt,
            &registry,
            tool_ctx,
            spawner,
            notifications,
            subagent_activity,
            loop_config,
            model_choices,
            services,
        )
        .await?;
        active_theme = app.theme;
        match exit {
            AppExit::Quit => return Ok(()),
            AppExit::Switch(next) => session_override = Some(next),
        }
    } // session loop
}

/// The meter must show the limit compaction actually measures against —
/// the provider's input cap, not the whole window. Showing the window
/// reads as comfortable headroom while the request is already too big.
fn display_context_limit(resolver: &dyn ProviderResolver, model: &str) -> Option<u64> {
    resolver
        .compaction_limit(model)
        .or_else(|| resolver.context_limit(model))
}

fn session_context_tokens(
    store: &SessionStore,
    session_id: &str,
    system_prompt: &str,
    registry: &ToolRegistry,
) -> Result<(u64, bool)> {
    let session = store.load(session_id)?;
    let estimated = ilar::compaction::estimate_reader_tokens_with_request(
        &session,
        Some(system_prompt),
        &registry.definitions(),
    );
    Ok((estimated, true))
}

struct PendingNotification {
    notification: ilar::subagent::Notification,
    queued_ahead: usize,
}

fn next_notification(
    turn_active: bool,
    paused: bool,
    pending: &mut Option<PendingNotification>,
    notifications: &mut tokio::sync::mpsc::Receiver<ilar::subagent::Notification>,
) -> Option<ilar::subagent::Notification> {
    if turn_active || paused {
        return None;
    }
    if pending
        .as_ref()
        .is_some_and(|pending| pending.queued_ahead == 0)
    {
        return pending.take().map(|pending| pending.notification);
    }
    match notifications.try_recv() {
        Ok(notification) => {
            if let Some(pending) = pending {
                pending.queued_ahead = pending.queued_ahead.saturating_sub(1);
            }
            Some(notification)
        }
        Err(_) => pending.take().map(|pending| pending.notification),
    }
}

enum TurnCompletion {
    Root(Result<TurnOutcome>),
    Routed(Result<ilar::subagent::RouteOutcome>),
}

fn ring_terminal_bell_if_idle(
    writer: &mut impl std::io::Write,
    pending: &mut bool,
    turn_active: bool,
) -> std::io::Result<bool> {
    if !*pending || turn_active {
        return Ok(false);
    }
    *pending = false;
    writer.write_all(b"\x07")?;
    writer.flush()?;
    Ok(true)
}

struct WheelBatch {
    rows: isize,
    deferred: Option<Event>,
}

fn wheel_rows(event: &Event) -> Option<isize> {
    match event {
        Event::Mouse(mouse) if mouse.kind == MouseEventKind::ScrollUp => Some(-3),
        Event::Mouse(mouse) if mouse.kind == MouseEventKind::ScrollDown => Some(3),
        _ => None,
    }
}

fn drain_wheel_batch(
    initial_rows: isize,
    max_events: usize,
    mut try_next: impl FnMut() -> Result<Option<Event>>,
) -> Result<WheelBatch> {
    let mut rows = initial_rows;
    let mut events = 1usize;
    while events < max_events.max(1) {
        let Some(event) = try_next()? else {
            return Ok(WheelBatch {
                rows,
                deferred: None,
            });
        };
        if let Some(next_rows) = wheel_rows(&event) {
            rows = rows.saturating_add(next_rows);
            events += 1;
        } else {
            return Ok(WheelBatch {
                rows,
                deferred: Some(event),
            });
        }
    }
    Ok(WheelBatch {
        rows,
        deferred: None,
    })
}

/// How run_app ended: quit the program, or restart against another session.
enum AppExit {
    Quit,
    Switch(String),
}

#[allow(clippy::too_many_arguments)]
async fn run_app(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    user_config_path: &std::path::Path,
    resolver: Arc<dyn ProviderResolver>,
    store: &SessionStore,
    session_id: &str,
    system_prompt: &str,
    registry: &ToolRegistry,
    tool_ctx: ToolContext,
    spawner: std::sync::Arc<ilar::subagent::SubagentSpawner>,
    mut notifications: tokio::sync::mpsc::Receiver<ilar::subagent::Notification>,
    mut subagent_activity: tokio::sync::broadcast::Receiver<ilar::subagent::SubagentActivity>,
    loop_config: LoopConfig,
    model_choices: Vec<&'static ilar::model::ModelInfo>,
    services: std::sync::Arc<ilar::tools::service::ServiceManager>,
) -> Result<AppExit> {
    let mut events_rx: Option<LoopEventReceiver> = None;
    let mut pending_notification = None;
    let mut notifications_paused = false;
    let mut cancel: Option<CancellationToken> = None;
    let mut turn_handle: Option<tokio::task::JoinHandle<TurnCompletion>> = None;
    let mut ring_on_turn_completion = false;
    let mut bell_pending = false;
    let mut pending_terminal_event = None;

    loop {
        // Drain pending loop events.
        if let Some(rx) = events_rx.as_mut() {
            while let Ok(event) = rx.try_recv() {
                app.push_loop_event(&event);
            }
        }
        app.retry_subagent_activity();
        for _ in 0..256 {
            let Ok(activity) = subagent_activity.try_recv() else {
                break;
            };
            app.push_subagent_activity(&activity);
        }
        // Turn finished? Join and clean up.
        if let Some(handle) = turn_handle.as_mut()
            && handle.is_finished()
        {
            let handle = turn_handle.take().unwrap();
            bell_pending |= std::mem::take(&mut ring_on_turn_completion);
            if let Some(rx) = events_rx.as_mut() {
                while let Ok(event) = rx.try_recv() {
                    app.push_loop_event(&event);
                }
            }
            match handle.await {
                Ok(TurnCompletion::Root(result)) => {
                    let aborted = matches!(result, Ok(TurnOutcome::Aborted));
                    let completed = matches!(result, Ok(TurnOutcome::Completed));
                    app.finish_turn(result);
                    if !aborted {
                        notifications_paused = false;
                    }
                    if aborted && let Some((_, round)) = &app.goal {
                        let message = format!(
                            "goal paused (round {round}/{MAX_GOAL_ROUNDS}) — resumes after your next completed turn; Ctrl-Q to manage"
                        );
                        app.push_transcript_line(Line_::System(message.clone()));
                        app.set_notice(message, NoticeLevel::Warning);
                    }
                    // Goal mode: verify-and-continue until the sentinel,
                    // a cap, or user interjections (queue wins below).
                    if completed
                        && app.queued_messages.is_empty()
                        && app.input.is_blank()
                        && !app.has_modal()
                        && pending_terminal_event.is_none()
                        && let Some((goal, round)) = app.goal.clone()
                    {
                        let achieved = app
                            .lines
                            .iter()
                            .rev()
                            .find_map(|line| match line {
                                Line_::Assistant(text) => Some(goal_achieved_in(text)),
                                _ => None,
                            })
                            .unwrap_or(false);
                        if achieved {
                            app.goal = None;
                            let message = format!("goal achieved after {} round(s)", round.max(1));
                            app.push_transcript_line(Line_::System(message.clone()));
                            app.set_notice(message, NoticeLevel::Info);
                        } else if round >= MAX_GOAL_ROUNDS {
                            app.goal = None;
                            let message = format!(
                                "goal round cap ({MAX_GOAL_ROUNDS}) reached without \
                                 {GOAL_SENTINEL} — stopping"
                            );
                            app.push_transcript_line(Line_::System(message.clone()));
                            app.set_notice(message, NoticeLevel::Warning);
                        } else {
                            let next_round = round + 1;
                            app.goal = Some((goal.clone(), next_round));
                            app.input =
                                InputBuffer::from(goal_continuation_prompt(&goal, next_round));
                            pending_terminal_event = Some(Event::Key(KeyEvent::new(
                                KeyCode::Enter,
                                KeyModifiers::NONE,
                            )));
                        }
                    }
                    if !app.queued_messages.is_empty() {
                        // Only dequeue into an idle, modal-free UI: a
                        // synthetic Enter routed into a picker or search
                        // bar would misfire (or lose the message), and a
                        // pending real event must not be clobbered.
                        if completed
                            && app.input.is_blank()
                            && !app.has_modal()
                            && pending_terminal_event.is_none()
                        {
                            let next = app.queued_messages.remove(0);
                            app.input = InputBuffer::from(next);
                            // Send through the ordinary submit path.
                            pending_terminal_event = Some(Event::Key(KeyEvent::new(
                                KeyCode::Enter,
                                KeyModifiers::NONE,
                            )));
                        } else {
                            app.set_notice(
                                format!(
                                    "{} queued message(s) held — Ctrl-Q to review",
                                    app.queued_messages.len()
                                ),
                                NoticeLevel::Warning,
                            );
                        }
                    }
                }
                Ok(TurnCompletion::Routed(Ok(ilar::subagent::RouteOutcome::Propagate(
                    notification,
                )))) => {
                    app.busy = false;
                    app.status = "ready".into();
                    app.clear_transient_notice();
                    app.set_activity(Activity::Ready);
                    pending_notification = Some(PendingNotification {
                        notification,
                        queued_ahead: notifications.len(),
                    });
                }
                Ok(TurnCompletion::Routed(Ok(ilar::subagent::RouteOutcome::Requeue(
                    notification,
                )))) => {
                    app.busy = false;
                    app.status = "notification paused; send a message to resume".into();
                    app.set_persistent_notice(
                        "notification paused; send a message to resume",
                        NoticeLevel::Warning,
                    );
                    app.set_activity(Activity::Paused);
                    pending_notification = Some(PendingNotification {
                        notification,
                        queued_ahead: 0,
                    });
                    notifications_paused = true;
                }
                Ok(TurnCompletion::Routed(Ok(ilar::subagent::RouteOutcome::Complete))) => {
                    app.busy = false;
                    app.status = "ready".into();
                    app.clear_transient_notice();
                    app.set_activity(Activity::Ready);
                }
                Ok(TurnCompletion::Routed(Err(error))) => {
                    app.busy = false;
                    app.status = "error".into();
                    app.set_activity(Activity::Error);
                    let message = format!("notification routing failed: {error}");
                    app.set_notice(&message, NoticeLevel::Error);
                    app.push_transcript_line(Line_::System(message));
                }
                Err(error) => {
                    app.busy = false;
                    app.status = "error".into();
                    app.set_activity(Activity::Error);
                    let message = format!("notification routing failed: {error}");
                    app.set_notice(&message, NoticeLevel::Error);
                    app.push_transcript_line(Line_::System(message));
                }
            }
            events_rx = None;
            cancel = None;
        }

        // Let a buffered Ctrl-P open the palette before starting queued work.
        let mut modal_open = app.has_modal();
        if turn_handle.is_none()
            && !notifications_paused
            && !modal_open
            && pending_terminal_event.is_none()
            && crossterm::event::poll(std::time::Duration::ZERO)?
        {
            pending_terminal_event = Some(crossterm::event::read()?);
        }
        if pending_terminal_event
            .as_ref()
            .is_some_and(is_command_palette_shortcut)
        {
            pending_terminal_event = None;
            app.model_key_pending = false;
            if app.search_active {
                app.close_search(false);
            }
            app.open_command_palette();
            modal_open = app.has_modal();
        }
        // Background completions re-invoke their declared parent while idle.
        if let Some(notification) = next_notification(
            turn_handle.is_some(),
            notifications_paused || modal_open,
            &mut pending_notification,
            &mut notifications,
        ) {
            if notification.parent_session_id != session_id {
                let token = CancellationToken::new();
                cancel = Some(token.clone());
                app.busy = true;
                app.status = format!("routing task to {}", notification.parent_session_id);
                app.clear_transient_notice();
                app.set_activity(Activity::Thinking);
                let spawner = spawner.clone();
                turn_handle = Some(tokio::spawn(async move {
                    TurnCompletion::Routed(spawner.route_notification(notification, token).await)
                }));
                continue;
            }
            let text = notification.text;
            app.push_notification(&notification.description, &text);
            let (tx, rx) = loop_event_channel(LOOP_EVENT_CAPACITY);
            events_rx = Some(rx);
            let token = CancellationToken::new();
            cancel = Some(token.clone());
            app.busy = true;
            app.status = "thinking".into();
            app.clear_transient_notice();
            app.set_activity(Activity::Thinking);
            let resolver = resolver.clone();
            let store = store.clone();
            let session_id = session_id.to_string();
            let system_prompt = system_prompt.to_string();
            let registry = registry.clone();
            let turn_ctx = tool_ctx.clone();
            let loop_config = loop_config.clone();
            turn_handle = Some(tokio::spawn(async move {
                TurnCompletion::Root(
                    run_turn(
                        resolver.as_ref(),
                        &registry,
                        &store,
                        &session_id,
                        &text,
                        Some(&system_prompt),
                        loop_config,
                        tx,
                        token,
                        turn_ctx,
                    )
                    .await,
                )
            }));
        }

        let _ = ring_terminal_bell_if_idle(
            &mut std::io::stdout(),
            &mut bell_pending,
            turn_handle.is_some(),
        );

        app.background_running = spawner.running_background();
        app.services_view = services.snapshot();
        app.services_running = app
            .services_view
            .iter()
            .filter(|(_, running, _)| *running)
            .count();
        terminal.draw(|frame| app.render(frame))?;

        // Poll terminal input (fast while busy so streaming keeps rendering).
        let timeout = if app.busy {
            std::time::Duration::from_millis(50)
        } else {
            std::time::Duration::from_millis(250)
        };
        let event = if let Some(event) = pending_terminal_event.take() {
            event
        } else {
            if !crossterm::event::poll(timeout)? {
                continue;
            }
            crossterm::event::read()?
        };
        match event {
            Event::Key(
                key @ KeyEvent {
                    code,
                    kind: KeyEventKind::Press | KeyEventKind::Repeat,
                    modifiers,
                    ..
                },
            ) => {
                let control = modifiers.contains(KeyModifiers::CONTROL);
                if matches!((code, control), (KeyCode::Char('c'), true)) {
                    if let Some(cancel) = &cancel {
                        cancel.cancel();
                    }
                    spawner.shutdown().await;
                    return Ok(AppExit::Quit);
                }
                if app.active_modal() == Some(Modal::PendingManager) {
                    match app.pending_manager_key(code, control) {
                        PendingAction::Stay => {}
                        PendingAction::Close => app.pending_manager = None,
                        PendingAction::DeleteQueued(index) => {
                            if index < app.queued_messages.len() {
                                let removed = app.queued_messages.remove(index);
                                app.set_notice(
                                    format!(
                                        "removed queued message: {}",
                                        removed.lines().next().unwrap_or("")
                                    ),
                                    NoticeLevel::Info,
                                );
                            }
                        }
                        PendingAction::EditQueued(index) => {
                            if index < app.queued_messages.len() {
                                let message = app.queued_messages.remove(index);
                                app.input = InputBuffer::from(message);
                                app.pending_manager = None;
                            }
                        }
                        PendingAction::AbortGoal => {
                            if let Some((goal, round)) = app.goal.take() {
                                let message =
                                    format!("goal aborted after {round} round(s): {goal}");
                                app.push_transcript_line(Line_::System(message.clone()));
                                app.set_notice(message, NoticeLevel::Info);
                            }
                        }
                        PendingAction::EditGoal => {
                            if let Some((goal, _)) = &app.goal {
                                app.input = InputBuffer::from(format!("/goal {goal}"));
                                app.pending_manager = None;
                            }
                        }
                        PendingAction::CancelBackground => {
                            spawner.abort_all();
                            notifications_paused = true;
                            app.background_running = 0;
                            app.set_persistent_notice(
                                "background jobs cancelled; notifications paused; send a message to resume",
                                NoticeLevel::Warning,
                            );
                        }
                        PendingAction::StopServices => {
                            services.stop_all();
                            app.services_running = 0;
                            app.set_notice("services stopped", NoticeLevel::Info);
                        }
                        PendingAction::DismissRetry => {
                            app.retry_available = false;
                            app.clear_transient_notice();
                        }
                        PendingAction::RetryNow => {
                            if !app.busy && turn_handle.is_none() {
                                pending_terminal_event = begin_retry(app);
                                if pending_terminal_event.is_some() {
                                    app.pending_manager = None;
                                }
                            }
                        }
                    }
                    continue;
                }
                if app.active_modal() == Some(Modal::Help) {
                    match code {
                        KeyCode::Up => app.help_scroll = app.help_scroll.saturating_sub(1),
                        KeyCode::Down => app.help_scroll = app.help_scroll.saturating_add(1),
                        KeyCode::PageUp => app.help_scroll = app.help_scroll.saturating_sub(10),
                        KeyCode::PageDown => app.help_scroll = app.help_scroll.saturating_add(10),
                        KeyCode::Esc | KeyCode::F(1) | KeyCode::Char('?' | 'q') => {
                            app.help_visible = false;
                            app.help_scroll = 0;
                        }
                        _ => {}
                    }
                    continue;
                }
                if app.active_modal() == Some(Modal::ThemePicker) {
                    let action = {
                        let picker = app.theme_picker.as_mut().unwrap();
                        picker.handle_key(code, control)
                    };
                    apply_theme_picker_action(app, action, |selected| {
                        ilar::config::persist_general_theme(user_config_path, selected.id())
                    });
                    continue;
                }
                if app.active_modal() == Some(Modal::SkillPicker)
                    && let Some(picker) = app.skill_picker.as_mut()
                {
                    match picker.handle_key(code, control) {
                        PickerAction::Stay => {}
                        PickerAction::Dismiss => {
                            app.skill_picker = None;
                        }
                        PickerAction::Choose(name) => {
                            app.skill_picker = None;
                            app.input = InputBuffer::from(format!("/{name} "));
                        }
                    }
                    continue;
                }
                if app.active_modal() == Some(Modal::SessionPicker)
                    && let Some(picker) = app.session_picker.as_mut()
                {
                    match picker.handle_key(code, control) {
                        SessionPickerAction::Stay => {}
                        SessionPickerAction::Dismiss => {
                            app.session_picker = None;
                            app.clear_transient_notice();
                        }
                        SessionPickerAction::Delete(id) => match store.delete(&id) {
                            Ok(()) => {
                                if let Some(picker) = app.session_picker.as_mut() {
                                    picker.sessions.retain(|session| session.id != id);
                                    picker.selected = 0;
                                }
                                app.set_notice(format!("deleted session {id}"), NoticeLevel::Info);
                            }
                            Err(error) => {
                                app.set_notice(
                                    format!("cannot delete {id}: {error}"),
                                    NoticeLevel::Error,
                                );
                            }
                        },
                        SessionPickerAction::Fork(id) => {
                            let blocked = if turn_handle.is_some() {
                                Some("finish or abort the current turn before switching sessions")
                            } else if spawner.running_background() > 0 {
                                Some("background agents are running; wait or abort them first")
                            } else if !app.input.is_blank() {
                                Some("input has an unsent draft; send or clear it first")
                            } else {
                                None
                            };
                            if let Some(reason) = blocked {
                                app.set_notice(reason, NoticeLevel::Warning);
                                continue;
                            }
                            match store.fork(&id) {
                                Ok(fork_id) => {
                                    spawner.shutdown().await;
                                    return Ok(AppExit::Switch(fork_id));
                                }
                                Err(error) => {
                                    app.set_notice(
                                        format!("cannot fork {id}: {error}"),
                                        NoticeLevel::Error,
                                    );
                                }
                            }
                        }
                        SessionPickerAction::Resume(new_session) => {
                            let blocked = if turn_handle.is_some() {
                                Some("finish or abort the current turn before switching sessions")
                            } else if spawner.running_background() > 0 {
                                Some("background agents are running; wait or abort them first")
                            } else if !app.input.is_blank() {
                                Some("input has an unsent draft; send or clear it first")
                            } else {
                                None
                            };
                            if let Some(reason) = blocked {
                                app.set_notice(reason, NoticeLevel::Warning);
                                continue;
                            }
                            // Validate now so a bad entry degrades to a
                            // notice instead of exiting the app later.
                            match store
                                .load(&new_session)
                                .map(|session| ensure_direct_resume_allowed(session.meta()))
                            {
                                Ok(Ok(())) => {
                                    spawner.shutdown().await;
                                    return Ok(AppExit::Switch(new_session));
                                }
                                Ok(Err(error)) => {
                                    app.set_notice(
                                        format!("cannot resume {new_session}: {error}"),
                                        NoticeLevel::Error,
                                    );
                                }
                                Err(error) => {
                                    app.set_notice(
                                        format!("cannot resume {new_session}: {error}"),
                                        NoticeLevel::Error,
                                    );
                                }
                            }
                        }
                    }
                    continue;
                }
                if app.active_modal() == Some(Modal::ModelPicker)
                    && let Some(picker) = app.model_picker.as_mut()
                {
                    match picker.handle_key(code, control) {
                        PickerAction::Stay => {}
                        PickerAction::Dismiss => {
                            app.model_picker = None;
                            app.status = "ready".into();
                            app.clear_transient_notice();
                        }
                        PickerAction::Choose(new_model) => {
                            if let Some(model) = ilar::model::find(&new_model)
                                && !model.variants().is_empty()
                            {
                                app.clear_transient_notice();
                                let active_variant = (new_model == app.current_model)
                                    .then_some(app.current_variant.as_deref())
                                    .flatten();
                                app.model_picker = None;
                                app.variant_picker =
                                    Some(VariantPicker::new(model, active_variant));
                                continue;
                            }
                            match adopt_model_selection(
                                app,
                                resolver.as_ref(),
                                store,
                                session_id,
                                system_prompt,
                                registry,
                                new_model.clone(),
                                None,
                            ) {
                                Ok(()) => {
                                    app.model_picker = None;
                                }
                                Err(error) => {
                                    if let Some(picker) = app.model_picker.as_mut() {
                                        picker.error =
                                            Some(format!("cannot switch to {new_model}: {error}"));
                                    }
                                }
                            }
                        }
                    }
                    continue;
                }
                if app.active_modal() == Some(Modal::VariantPicker) {
                    let (action, model) = {
                        let picker = app.variant_picker.as_mut().unwrap();
                        (picker.handle_key(code, control), picker.model.full_id())
                    };
                    match action {
                        VariantPickerAction::Stay => {}
                        VariantPickerAction::Dismiss => {
                            app.variant_picker = None;
                            app.clear_transient_notice();
                        }
                        VariantPickerAction::Choose(variant) => {
                            match adopt_model_selection(
                                app,
                                resolver.as_ref(),
                                store,
                                session_id,
                                system_prompt,
                                registry,
                                model.clone(),
                                variant,
                            ) {
                                Ok(()) => app.variant_picker = None,
                                Err(error) => {
                                    if let Some(picker) = app.variant_picker.as_mut() {
                                        picker.error = Some(format!(
                                            "cannot switch reasoning for {model}: {error}"
                                        ));
                                    }
                                }
                            }
                        }
                    }
                    continue;
                }
                if app.active_modal() == Some(Modal::Search) {
                    match (code, control) {
                        (KeyCode::Esc, _) => app.close_search(true),
                        (KeyCode::Enter, _) => app.close_search(false),
                        (KeyCode::Up, _) | (KeyCode::Char('p'), true) => app.search_jump(-1),
                        (KeyCode::Down, _) | (KeyCode::Char('n'), true) => app.search_jump(1),
                        (KeyCode::Char('f'), true) => app.close_search(false),
                        (KeyCode::Backspace, _) => {
                            app.search_query.pop();
                            app.search_refresh();
                        }
                        (KeyCode::Char(character), false) if !character.is_control() => {
                            app.search_query.push(character);
                            app.search_refresh();
                        }
                        _ => {}
                    }
                    continue;
                }
                if app.active_modal() == Some(Modal::CommandPalette)
                    && let Some(palette) = app.command_palette.as_mut()
                {
                    match palette.handle_key(code, control) {
                        CommandPaletteAction::Stay => {}
                        CommandPaletteAction::Dismiss => {
                            app.command_palette = None;
                        }
                        CommandPaletteAction::Choose(command) => {
                            activate_palette_command(app, command, model_choices.clone());
                            if std::mem::take(&mut app.session_picker_requested) {
                                let sessions = store
                                    .list()
                                    .into_iter()
                                    .filter(|session| session.id != app.session_id)
                                    .collect();
                                app.session_picker = Some(SessionPicker::new(sessions));
                            }
                        }
                    }
                    continue;
                }
                if matches!((code, control), (KeyCode::Char('p'), true)) {
                    app.model_key_pending = false;
                    app.open_command_palette();
                    continue;
                }
                if app.model_key_pending {
                    app.model_key_pending = false;
                    if code == KeyCode::Esc {
                        app.status = "ready".into();
                        app.clear_transient_notice();
                        continue;
                    }
                    if matches!(code, KeyCode::Char('m' | 'M'))
                        && !app.busy
                        && !model_choices.is_empty()
                    {
                        app.clear_transient_notice();
                        app.model_picker =
                            Some(ModelPicker::new(model_choices.clone(), &app.current_model));
                        continue;
                    }
                    if matches!(code, KeyCode::Char('t' | 'T')) && !app.busy {
                        app.clear_transient_notice();
                        app.theme_picker = Some(ThemePicker::new(app.theme));
                        continue;
                    }
                    app.status = "ready".into();
                    app.clear_transient_notice();
                }
                if matches!((code, control), (KeyCode::Char('x'), true)) && !app.busy {
                    app.model_key_pending = true;
                    app.status = "Ctrl-X: M models · T themes".into();
                    app.set_notice("Ctrl-X: M models · T themes", NoticeLevel::Info);
                    continue;
                }
                match (code, control) {
                    (KeyCode::F(1), _) => {
                        app.help_visible = true;
                        app.help_scroll = 0;
                    }
                    // Ctrl-M is simply unreachable without keyboard
                    // enhancement (the terminal reports it as Enter);
                    // the arm stays for terminals that do report it.
                    (KeyCode::Char('m'), true) | (KeyCode::F(2), false)
                        if !app.busy && !model_choices.is_empty() =>
                    {
                        app.clear_transient_notice();
                        app.model_picker =
                            Some(ModelPicker::new(model_choices.clone(), &app.current_model));
                    }
                    (KeyCode::F(3), false) if !app.busy => {
                        app.clear_transient_notice();
                        app.theme_picker = Some(ThemePicker::new(app.theme));
                    }
                    (KeyCode::Esc, _) => {
                        // Esc is strictly immediate-scope: abort the running
                        // turn or clear the input. Standing state (goal,
                        // queue, background jobs) lives in the pending
                        // manager (Ctrl-Q) and explicit commands.
                        if app.busy {
                            if let Some(cancel) = &cancel {
                                cancel.cancel();
                                app.status = "aborting…".into();
                                app.set_notice("aborting turn…", NoticeLevel::Warning);
                                app.set_activity(Activity::Aborting);
                            }
                        } else if !app.input.is_blank() {
                            app.input.clear();
                        }
                    }
                    // Ctrl-U edits the input when it has text; the
                    // half-page scroll needs a blank input.
                    (KeyCode::Char('u'), true) if app.input.is_blank() => {
                        app.scroll_up(app.page_size().div_ceil(2));
                    }
                    (KeyCode::Char('d'), true) if app.input.is_blank() => {
                        app.scroll_down(app.page_size().div_ceil(2));
                    }
                    (KeyCode::Home, true) => app.scroll_to_top(),
                    (KeyCode::End, true) => app.scroll_to_tail(),
                    (KeyCode::PageUp, _) => app.scroll_up(app.page_size()),
                    (KeyCode::PageDown, _) => app.scroll_down(app.page_size()),
                    // History recall wins while browsing or on a blank
                    // input; transcript scrolling stays on PgUp/wheel/^U.
                    (KeyCode::Up, _)
                        if app.history.browsing()
                            || (!app.input.is_multiline() && app.input.is_blank()) =>
                    {
                        if let Some(text) = app.history.previous(app.input.text()) {
                            app.input = InputBuffer::from(text);
                        } else if !app.history.browsing() {
                            app.scroll_up(1);
                        }
                    }
                    (KeyCode::Down, _) if app.history.browsing() => {
                        if let Some(text) = app.history.next(app.input.text()) {
                            app.input = InputBuffer::from(text);
                        }
                    }
                    (KeyCode::Up, _) if !app.input.is_multiline() => app.scroll_up(1),
                    (KeyCode::Down, _) if !app.input.is_multiline() => app.scroll_down(1),
                    (KeyCode::Char('f'), true) => {
                        app.open_search();
                    }
                    (KeyCode::Char('q'), true) => {
                        app.pending_manager = Some(PendingManager::default());
                    }
                    (code, control)
                        if retry_requested(code, control)
                            && app.retry_available
                            && !app.busy
                            && turn_handle.is_none() =>
                    {
                        pending_terminal_event = begin_retry(app);
                    }
                    // Inline slash completion: navigate/accept while the
                    // command name is being typed.
                    (KeyCode::Up | KeyCode::Down | KeyCode::Tab | KeyCode::Enter, false)
                        if !slash_candidates(app.input.text(), &app.skills).is_empty()
                            && !key.modifiers.contains(KeyModifiers::SHIFT) =>
                    {
                        let candidates = slash_candidates(app.input.text(), &app.skills);
                        app.slash_selected = app.slash_selected.min(candidates.len() - 1);
                        match code {
                            KeyCode::Up => {
                                app.slash_selected =
                                    (app.slash_selected + candidates.len() - 1) % candidates.len();
                            }
                            KeyCode::Down => {
                                app.slash_selected = (app.slash_selected + 1) % candidates.len();
                            }
                            // Tab and Enter both accept the selection; the
                            // completed input ("/name ") hides the popup, so
                            // a second Enter submits as usual.
                            KeyCode::Tab | KeyCode::Enter => {
                                let (name, _) = &candidates[app.slash_selected];
                                app.input = InputBuffer::from(format!("/{name} "));
                                app.slash_selected = 0;
                            }
                            _ => {}
                        }
                    }
                    _ => match handle_prompt_key(&mut app.input, key) {
                        PromptAction::Submit
                            if turn_handle.is_none() && !app.busy && !app.input.is_blank() =>
                        {
                            let mut text = app.input.take();
                            app.history.push(&text);
                            app.last_prompt = Some(text.clone());
                            app.retry_available = false;
                            if let Some(("goal", goal_text)) = parse_slash_invocation(&text) {
                                if goal_text.is_empty() {
                                    match &app.goal {
                                        Some((goal, _)) => {
                                            // Prefill for editing; Esc on an
                                            // emptied input aborts instead.
                                            app.input = InputBuffer::from(format!("/goal {goal}"));
                                            app.set_notice(
                                                "edit the goal and press Enter — /goal abort ends it",
                                                NoticeLevel::Info,
                                            );
                                        }
                                        None => app.set_notice(
                                            "no active goal — /goal <description> sets one",
                                            NoticeLevel::Info,
                                        ),
                                    }
                                    continue;
                                }
                                if goal_text == "abort" {
                                    let notice = match app.goal.take() {
                                        Some((goal, round)) => {
                                            let message = format!(
                                                "goal aborted after {round} round(s): {goal}"
                                            );
                                            app.push_transcript_line(Line_::System(
                                                message.clone(),
                                            ));
                                            message
                                        }
                                        None => "no active goal".into(),
                                    };
                                    app.set_notice(notice, NoticeLevel::Info);
                                    continue;
                                }
                                if let Some((goal, round)) = &mut app.goal {
                                    // Editing mid-loop keeps the round
                                    // budget; the next continuation carries
                                    // the new wording.
                                    if goal != goal_text {
                                        *goal = goal_text.to_string();
                                        let round = *round;
                                        app.push_transcript_line(Line_::System(format!(
                                            "goal updated (round {round}/{MAX_GOAL_ROUNDS}): {goal_text}"
                                        )));
                                    }
                                    app.clear_transient_notice();
                                    continue;
                                }
                                app.goal = Some((goal_text.to_string(), 0));
                                app.push_transcript_line(Line_::System(format!(
                                    "goal armed (max {MAX_GOAL_ROUNDS} rounds): {goal_text}"
                                )));
                                text = goal_kickoff_prompt(goal_text);
                            } else if let Some((name, args)) = parse_slash_invocation(&text) {
                                if app.skills.iter().any(|(skill, _)| skill == name) {
                                    text = skill_invocation_prompt(name, args);
                                } else {
                                    let matches = close_skill_matches(&app.skills, name);
                                    app.input = InputBuffer::from(text.as_str());
                                    app.set_notice(
                                        format!(
                                            "unknown skill /{name} · available: {}",
                                            matches.join(", ")
                                        ),
                                        NoticeLevel::Warning,
                                    );
                                    continue;
                                }
                            }
                            app.clear_notice();
                            app.push_transcript_line(Line_::User(text.clone()));
                            app.follow_tail = true;
                            let (tx, rx) = loop_event_channel(LOOP_EVENT_CAPACITY);
                            events_rx = Some(rx);
                            let token = CancellationToken::new();
                            cancel = Some(token.clone());
                            app.busy = true;
                            app.status = "thinking".into();
                            app.set_activity(Activity::Thinking);
                            let resolver = resolver.clone();
                            let store = store.clone();
                            let session_id = session_id.to_string();
                            let system_prompt = system_prompt.to_string();
                            let registry = registry.clone();
                            let turn_ctx = tool_ctx.clone();
                            let mut loop_config = loop_config.clone();
                            if std::mem::take(&mut app.compact_requested) {
                                loop_config.force_compaction = true;
                            }
                            ring_on_turn_completion = true;
                            turn_handle = Some(tokio::spawn(async move {
                                TurnCompletion::Root(
                                    run_turn(
                                        resolver.as_ref(),
                                        &registry,
                                        &store,
                                        &session_id,
                                        &text,
                                        Some(&system_prompt),
                                        loop_config,
                                        tx,
                                        token,
                                        turn_ctx,
                                    )
                                    .await,
                                )
                            }));
                        }
                        PromptAction::Submit
                            if (turn_handle.is_some() || app.busy) && !app.input.is_blank() =>
                        {
                            let text = app.input.take();
                            app.history.push(&text);
                            app.queued_messages.push(text);
                            app.set_notice(
                                format!(
                                    "queued ({} waiting) — sends when the turn completes",
                                    app.queued_messages.len()
                                ),
                                NoticeLevel::Info,
                            );
                        }
                        PromptAction::Edited => app.clear_transient_notice(),
                        PromptAction::Unhandled | PromptAction::Submit => {}
                    },
                }
            }
            Event::Paste(text) if app.active_modal() == Some(Modal::CommandPalette) => {
                app.command_palette.as_mut().unwrap().insert_query(&text);
            }
            Event::Paste(text) if app.active_modal() == Some(Modal::Search) => {
                app.search_query.push_str(text.trim());
                app.search_refresh();
            }
            Event::Paste(text) if !app.has_modal() => {
                app.model_key_pending = false;
                app.clear_transient_notice();
                app.input.insert(&text);
            }
            Event::Mouse(mouse)
                if matches!(
                    mouse.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ) =>
            {
                let initial_rows = if mouse.kind == MouseEventKind::ScrollUp {
                    -3
                } else {
                    3
                };
                let batch = drain_wheel_batch(initial_rows, MAX_WHEEL_EVENTS_PER_BATCH, || {
                    if crossterm::event::poll(std::time::Duration::ZERO)? {
                        Ok(Some(crossterm::event::read()?))
                    } else {
                        Ok(None)
                    }
                })?;
                pending_terminal_event = batch.deferred;
                // The overlay in front gets first refusal; a 45-entry
                // model picker should scroll like everything else.
                if !app.scroll_active_modal(batch.rows) {
                    app.scroll_wheel(batch.rows);
                }
            }
            Event::Mouse(mouse)
                if app
                    .active_modal()
                    .is_none_or(|modal| modal == Modal::Search) =>
            {
                // Search is a transcript-reading mode, so selecting and
                // expanding must keep working underneath it.
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        app.begin_transcript_selection(mouse.column, mouse.row);
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        app.drag_transcript_selection(mouse.column, mouse.row);
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        if let Some(text) = app.finish_transcript_selection(mouse.column, mouse.row)
                            && let Err(error) = app.copy_to_clipboard(&text)
                        {
                            let message = format!("clipboard copy failed: {error:#}");
                            app.set_notice(&message, NoticeLevel::Error);
                            app.push_transcript_line(Line_::System(message));
                            app.follow_tail = true;
                            app.set_activity(Activity::Error);
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modals::{PALETTE_COMMANDS, PaletteAction, PaletteCommand, ThemePickerAction};
    use crate::session_view::restored_session_view;
    use crate::text::tests::rendered_text;
    use crate::transcript::*;

    /// Render precedence and key-dispatch precedence used to be two
    /// hand-maintained orders that were near-opposite. One value now
    /// feeds both, so they cannot disagree about which overlay is in
    /// front of the user.
    #[test]
    fn one_precedence_order_decides_the_active_overlay() {
        let mut app = App::new();
        assert_eq!(app.active_modal(), None);
        assert!(!app.has_modal());

        app.model_picker = Some(ModelPicker::new(
            ilar::model::catalog().iter().collect(),
            "zai/glm-4.7",
        ));
        assert_eq!(app.active_modal(), Some(Modal::ModelPicker));

        // The pending manager is reachable from the palette and outranks
        // everything: whatever is showing must also be taking the keys.
        app.pending_manager = Some(PendingManager::default());
        assert_eq!(app.active_modal(), Some(Modal::PendingManager));
    }

    /// Search owns the keyboard like any other overlay. It used to sit
    /// outside `has_modal`, so a paste landed in the message input, the
    /// input kept a caret it was not receiving, and a background
    /// notification could start a turn underneath the search bar.
    #[test]
    fn search_is_a_modal_like_any_other() {
        let mut app = App::new();
        app.open_search();
        assert_eq!(app.active_modal(), Some(Modal::Search));
        assert!(app.has_modal());
        assert!(
            !input_accepts_keys(false, app.has_modal()),
            "the prompt must not show a caret while search takes the keys"
        );
        app.close_search(true);
        assert!(!app.has_modal());
    }

    /// Everything else in the transcript is mouse-driven; a 45-entry
    /// model picker that cannot be scrolled is the odd one out.
    #[test]
    fn the_wheel_scrolls_the_active_picker() {
        let mut app = App::new();
        // Pin the entry count so catalog reordering cannot flip this.
        let models: Vec<_> = ilar::model::catalog().iter().take(10).collect();
        let first = models[0].full_id();
        app.model_picker = Some(ModelPicker::new(models, &first));
        assert_eq!(app.model_picker.as_ref().unwrap().selected, 0);

        assert!(app.scroll_active_modal(3));
        assert_eq!(app.model_picker.as_ref().unwrap().selected, 3);
        assert!(app.scroll_active_modal(-3));
        assert_eq!(app.model_picker.as_ref().unwrap().selected, 0);
    }

    /// The theme picker previews the highlighted theme across the whole
    /// UI and its footer says so, so the wheel must preview like the
    /// arrow keys rather than only moving the marker.
    #[test]
    fn the_wheel_previews_themes_like_the_arrow_keys() {
        let mut app = App::new();
        app.theme = theme::ThemeId::ALL[0];
        app.theme_picker = Some(ThemePicker::new(app.theme));

        assert!(app.scroll_active_modal(1));
        let highlighted = app.theme_picker.as_ref().unwrap().selected_theme();
        assert_ne!(highlighted, theme::ThemeId::ALL[0]);
        assert_eq!(
            app.theme, highlighted,
            "the wheel moved the marker without previewing the theme"
        );
    }

    /// A net-zero wheel batch has to fall through to the transcript:
    /// `scroll_wheel` is what clears a stale selection.
    #[test]
    fn a_net_zero_wheel_batch_is_not_consumed_by_a_modal() {
        let mut app = App::new();
        assert!(!app.scroll_active_modal(0));
        app.open_search();
        assert!(!app.scroll_active_modal(0));
        app.close_search(true);
        app.help_visible = true;
        assert!(!app.scroll_active_modal(0));
    }

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

    /// Crossterm maps CR to Enter before the control-character branch, so
    /// without the kitty protocol Ctrl-M *is* Enter and would fire off the
    /// draft. Do not advertise it there.

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

    /// The meter must not show the whole window while compaction is
    /// measuring against the input cap — that reads as comfortable
    /// headroom when the request is already too big to send.
    #[test]
    fn context_meter_uses_the_same_limit_as_compaction() {
        struct SplitLimits;
        impl ProviderResolver for SplitLimits {
            fn resolve_provider(&self, _: &str) -> Result<ilar::provider::ProviderHandle<'_>> {
                anyhow::bail!("unused")
            }
            fn context_limit(&self, _: &str) -> Option<u64> {
                Some(128_000)
            }
            fn input_limit(&self, _: &str) -> Option<u64> {
                Some(100_000)
            }
        }
        assert_eq!(
            display_context_limit(&SplitLimits, "openai/gpt-5.3-codex-spark"),
            Some(100_000),
            "meter showed the full window instead of the input cap"
        );

        struct WindowOnly;
        impl ProviderResolver for WindowOnly {
            fn resolve_provider(&self, _: &str) -> Result<ilar::provider::ProviderHandle<'_>> {
                anyhow::bail!("unused")
            }
            fn context_limit(&self, _: &str) -> Option<u64> {
                Some(64_000)
            }
        }
        assert_eq!(
            display_context_limit(&WindowOnly, "custom/model"),
            Some(64_000)
        );
    }

    #[test]
    fn startup_selection_respects_cli_and_persisted_precedence() {
        assert_eq!(selected_agent_name(None, None), "build");
        assert_eq!(selected_agent_name(None, Some("explore")), "explore");
        assert_eq!(
            selected_agent_name(Some("review"), Some("explore")),
            "review"
        );

        assert_eq!(
            selected_model(None, None, Some("zai/agent-model"), "zai/general"),
            "zai/agent-model"
        );
        assert_eq!(
            selected_model(
                Some("openai/cli"),
                None,
                Some("zai/agent-model"),
                "zai/general"
            ),
            "openai/cli"
        );
        assert_eq!(
            selected_model(
                None,
                Some("openai/persisted"),
                Some("zai/agent-model"),
                "zai/general"
            ),
            "openai/persisted"
        );
    }

    #[test]
    fn restored_task_notifications_are_not_attributed_to_the_user() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let session_id = new_id();
        let mut session = store
            .create(SessionMeta {
                session_id: session_id.clone(),
                parent_id: None,
                agent: "build".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::UserMessage {
                id: new_id(),
                text: "hello".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::UserMessage {
                id: new_id(),
                text: "<task-notification>\nTask \"Assess architecture and risks\" completed.\n<result>\nRepository review\n</result>\n</task-notification>".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::UserMessage {
                id: new_id(),
                text: "<tool-notification>\nBackground job job-1 (\"Run checks\") completed.\n<result>\nchecks passed\n</result>\n</tool-notification>".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(session);

        let view = restored_session_view(&store.load(&session_id).unwrap());
        let now = std::time::Instant::now();
        let rendered = view
            .lines
            .iter()
            .flat_map(|line| transcript_entry_lines(line, 100, now, now))
            .map(|line| rendered_text(&line))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("you  hello"), "{rendered}");
        assert!(
            rendered.contains("task ▸ Assess architecture and risks completed."),
            "{rendered}"
        );
        // The body is collapsed behind the disclosure.
        assert!(
            !rendered.contains("Repository review"),
            "body must be collapsed: {rendered}"
        );
        assert!(rendered.contains("more line(s)"), "{rendered}");
        assert!(!rendered.contains("you  Task"), "{rendered}");
        assert!(
            rendered.contains("job  ▸ job-1 (\"Run checks\") completed."),
            "{rendered}"
        );
        assert!(!rendered.contains("you  Background job"), "{rendered}");
        assert!(!rendered.contains("<task-notification>"), "{rendered}");
        assert!(!rendered.contains("<tool-notification>"), "{rendered}");
        assert!(!rendered.contains("<result>"), "{rendered}");

        let mut app = App::new();
        app.push_notification(
            "Live review",
            "<task-notification>\nTask \"Live review\" completed.\n<result>\nDone\n</result>\n</task-notification>",
        );
        let rendered = app
            .transcript_lines(100, now)
            .iter()
            .map(rendered_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rendered.contains("task ▸ Live review completed."),
            "{rendered}"
        );
        assert!(!rendered.contains("\nDone"), "collapsed body: {rendered}");
        // Clicking the header expands the body, a second click collapses.
        let task_id = app
            .lines
            .iter()
            .find_map(|line| match line {
                Line_::Task { id, .. } => Some(id.clone()),
                _ => None,
            })
            .expect("task line present");
        let target = TranscriptHitTarget::Thought(task_id);
        app.toggle_transcript_target(target.clone());
        let expanded = app
            .transcript_lines(100, now)
            .iter()
            .map(rendered_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(expanded.contains("Done"), "{expanded}");
        app.toggle_transcript_target(target);
        assert!(!rendered.contains("you  "), "{rendered}");
        assert!(!rendered.contains("<result>"), "{rendered}");

        let parsed = task_notification_display(
            "<task-notification>\nTask \"Review \"risky\" paths\" completed.\n<result>\nLiteral delimiters:\n<result>\ninside\n</result>\n</result>\n</task-notification>",
        )
        .unwrap();
        assert!(
            parsed.starts_with("Review \"risky\" paths completed."),
            "{parsed}"
        );
        assert!(parsed.contains("<result>\ninside\n</result>"), "{parsed}");
        assert_eq!(
            task_notification_display(
                "<task-notification>\nTask \"Build project\" failed: path \"foo\" is unavailable\n</task-notification>"
            )
            .unwrap(),
            "Build project failed: path \"foo\" is unavailable"
        );

        let mut fallback = App::new();
        fallback.push_notification("Unknown format", "opaque notification");
        let rendered = fallback
            .transcript_lines(100, now)
            .iter()
            .map(rendered_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rendered.contains("task notification: Unknown format"),
            "{rendered}"
        );
        assert!(rendered.contains("you  opaque notification"), "{rendered}");
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

    #[test]
    fn multiline_input_renders_multiple_lines_and_cursor_position() {
        let backend = ratatui::backend::TestBackend::new(40, 12);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.input = InputBuffer::from("first line\nsecond line\nthird line");

        terminal.draw(|frame| app.render(frame)).unwrap();

        let screen = (0..terminal.backend().buffer().area.height)
            .map(|row| {
                (0..terminal.backend().buffer().area.width)
                    .map(|column| terminal.backend().buffer()[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains("first line"), "{screen}");
        assert!(screen.contains("second line"), "{screen}");
        assert!(screen.contains("third line"), "{screen}");
        assert!(screen.contains("3/3"), "{screen}");
    }

    #[test]
    fn session_usage_accumulates_across_steps_and_poisons_on_unknown_pricing() {
        let mut app = App::new();
        app.current_model = "zai/glm-4.7".into();
        let step = ilar::session::Usage {
            input_tokens: 1_000_000,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            input_token_accounting: None,
        };
        for _ in 0..2 {
            app.push_loop_event(&LoopEvent::StepComplete {
                stop_reason: "end_turn".into(),
                usage: step,
            });
        }
        assert_eq!(app.session_usage.input_tokens, 2_000_000);
        let cost = app.session_cost.unwrap();
        assert!((cost - 1.2).abs() < 1e-9, "{cost}");

        // A step on an unpriced model keeps tokens but drops the dollars.
        app.current_model = "custom/self-hosted".into();
        app.push_loop_event(&LoopEvent::StepComplete {
            stop_reason: "end_turn".into(),
            usage: step,
        });
        assert_eq!(app.session_usage.input_tokens, 3_000_000);
        assert_eq!(app.session_cost, None);
    }

    #[test]
    fn command_palette_sizes_to_show_every_command() {
        let mut app = App::new();
        app.command_palette = Some(CommandPalette::new(palette_items()));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let screen = (0..24)
            .map(|row| {
                (0..80)
                    .map(|column| terminal.backend().buffer()[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        for definition in PALETTE_COMMANDS {
            assert!(
                screen.contains(definition.label),
                "missing {:?}:\n{screen}",
                definition.label
            );
        }
        assert!(
            !screen.contains("more"),
            "nothing should be clipped:\n{screen}"
        );
    }

    #[test]
    fn activity_line_carries_stream_liveness() {
        let now = std::time::Instant::now();
        let started = now - std::time::Duration::from_secs(1);
        let fresh = activity_line(true, Activity::Thinking, now, started, Some("2.0 KiB"))
            .expect("busy thinking renders");
        let fresh = rendered_text(&fresh);
        assert!(fresh.contains("thinking… · 2.0 KiB"), "{fresh}");

        let stalled = activity_line(
            true,
            Activity::Thinking,
            now,
            started,
            Some("2.0 KiB · no data 7s"),
        )
        .expect("busy thinking renders");
        assert!(rendered_text(&stalled).contains("no data 7s"));

        // Tool activity keeps its own label; liveness is not appended.
        let tools = activity_line(true, Activity::Tools, now, started, Some("2.0 KiB"))
            .expect("busy tools renders");
        assert!(!rendered_text(&tools).contains("KiB"));

        // The helper itself: fresh vs stalled vs absent.
        assert_eq!(stream_liveness(2048, None, None, now), None);
        assert_eq!(
            stream_liveness(2048, Some(now), None, now).as_deref(),
            Some("2.0 KiB")
        );
        assert_eq!(
            stream_liveness(2048, Some(now), Some(512.0), now).as_deref(),
            Some("2.0 KiB · 512 B/s")
        );
        assert_eq!(
            stream_liveness(
                0,
                Some(now - std::time::Duration::from_secs(7)),
                Some(512.0),
                now
            )
            .as_deref(),
            Some("0 B · no data 7s")
        );
    }

    #[test]
    fn transcript_search_finds_jumps_and_restores() {
        let mut app = App::new();
        app.lines = (0..40)
            .map(|index| Line_::User(format!("message number {index}")))
            .chain(std::iter::once(Line_::Assistant(
                "the special needle answer".into(),
            )))
            .collect();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let scroll_before = app.scroll_top;

        app.open_search();
        for character in "needle".chars() {
            app.search_query.push(character);
        }
        app.search_refresh();
        assert_eq!(app.search_matches.len(), 1, "{:?}", app.search_matches);
        assert!(!app.follow_tail);

        // Status line reflects the query and counter.
        let bar = rendered_text(&app.status_line(120));
        assert!(bar.contains("/needle"), "{bar}");
        assert!(bar.contains("1/1"), "{bar}");

        // Esc restores the pre-search view.
        app.close_search(true);
        assert!(!app.search_active);
        assert_eq!(app.scroll_top, scroll_before.min(app.max_scroll()));

        // Case-insensitive; no matches reported gracefully.
        app.open_search();
        app.search_query = "NEEDLE".into();
        app.search_refresh();
        assert_eq!(app.search_matches.len(), 1);
        app.search_query = "zzz-not-there".into();
        app.search_refresh();
        assert!(app.search_matches.is_empty());
        assert!(rendered_text(&app.status_line(120)).contains("no matches"));
    }

    #[test]
    fn slash_input_shows_inline_completion_including_goal() {
        let skills = vec![
            ("deploy".to_string(), "Deploy things".to_string()),
            ("greptile".to_string(), "Review comments".to_string()),
        ];
        // All candidates on bare slash, fuzzy-filtered as the name grows.
        let all = slash_candidates("/", &skills);
        assert_eq!(all.len(), 3);
        assert!(all.iter().any(|(name, _)| name == "goal"));
        let filtered = slash_candidates("/go", &skills);
        assert_eq!(
            filtered.first().map(|(name, _)| name.as_str()),
            Some("goal")
        );
        // Finished name (whitespace) or non-slash input: no popup.
        assert!(slash_candidates("/goal recover", &skills).is_empty());
        assert!(slash_candidates("plain text", &skills).is_empty());
        assert!(slash_candidates("/zzz", &skills).is_empty());

        // The popup renders above the input.
        let mut app = App::new();
        app.skills = skills;
        app.input = InputBuffer::from("/go");
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let screen = (0..24)
            .map(|row| {
                (0..80)
                    .map(|column| terminal.backend().buffer()[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains("/goal — work until"), "{screen}");
    }

    #[test]
    fn goal_sentinel_detection_requires_line_start() {
        assert!(goal_achieved_in(
            "done!\nGOAL_ACHIEVED: 5/5 turns replay at 92%"
        ));
        assert!(goal_achieved_in("  GOAL_ACHIEVED: evidence attached"));
        assert!(!goal_achieved_in(
            "we still need to reach GOAL_ACHIEVED status later"
        ));
        assert!(!goal_achieved_in("no sentinel here"));

        let kickoff = goal_kickoff_prompt("replay 5 turns at 90%");
        assert!(kickoff.contains("replay 5 turns at 90%"));
        assert!(kickoff.contains("evidence"), "{kickoff}");
        let cont = goal_continuation_prompt("replay 5 turns at 90%", 3);
        assert!(cont.contains("round 3/25"), "{cont}");
        assert!(cont.contains("GOAL_ACHIEVED"), "{cont}");
    }

    #[test]
    fn pending_manager_lists_and_mutates_standing_state() {
        let mut app = App::new();
        app.queued_messages = vec!["first".into(), "second".into()];
        app.goal = Some(("recover the engine".into(), 2));
        app.background_running = 1;
        app.retry_available = true;
        app.last_prompt = Some("previous prompt".into());
        app.pending_manager = Some(PendingManager::default());
        assert_eq!(app.pending_items().len(), 5);

        // Deleting a queued message is immediate and targeted.
        assert_eq!(
            app.pending_manager_key(KeyCode::Char('d'), false),
            PendingAction::DeleteQueued(0)
        );
        app.queued_messages.remove(0);
        assert_eq!(app.pending_items().len(), 4);

        // Enter on a queued message edits it into the input.
        assert_eq!(
            app.pending_manager_key(KeyCode::Enter, false),
            PendingAction::EditQueued(0)
        );

        // Goal abort requires arming: first d stays, second fires.
        app.pending_manager_key(KeyCode::Down, false);
        assert_eq!(
            app.pending_manager_key(KeyCode::Char('d'), false),
            PendingAction::Stay
        );
        assert_eq!(
            app.pending_manager_key(KeyCode::Char('d'), false),
            PendingAction::AbortGoal
        );
        // Moving the selection disarms.
        app.pending_manager_key(KeyCode::Char('d'), false);
        app.pending_manager_key(KeyCode::Down, false);
        assert_eq!(
            app.pending_manager_key(KeyCode::Char('d'), false),
            PendingAction::Stay,
            "background cancel must re-arm after selection moved"
        );
        assert_eq!(
            app.pending_manager_key(KeyCode::Esc, false),
            PendingAction::Close
        );
    }

    #[test]
    fn services_show_in_the_sidebar() {
        let mut app = App::new();
        app.services_view = vec![
            ("web".into(), true, "up 3m2s".into()),
            ("worker".into(), false, "exit 1".into()),
        ];
        app.services_running = 1;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 30)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let screen = (0..30)
            .map(|row| {
                (0..140)
                    .map(|column| terminal.backend().buffer()[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains("services (1)"), "{screen}");
        assert!(screen.contains("web · up 3m2s"), "{screen}");
        assert!(screen.contains("worker · exit 1"), "{screen}");
        assert!(screen.contains("todos"), "{screen}");
    }

    #[test]
    fn goal_shows_in_the_sidebar_on_wide_terminals() {
        let mut app = App::new();
        app.goal = Some((
            "recover the engine until 5 turns replay at 90% accuracy".into(),
            4,
        ));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 30)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let screen = (0..30)
            .map(|row| {
                (0..140)
                    .map(|column| terminal.backend().buffer()[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains("goal 4/25"), "{screen}");
        assert!(screen.contains("recover the engine"), "{screen}");
        assert!(screen.contains("Ctrl-Q manage"), "{screen}");
        assert!(
            screen.contains("todos"),
            "todos panel still present: {screen}"
        );
    }

    #[test]
    fn goal_round_shows_in_the_input_title() {
        let mut app = App::new();
        app.goal = Some(("recover the engine".into(), 3));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let screen = (0..24)
            .map(|row| {
                (0..80)
                    .map(|column| terminal.backend().buffer()[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains("goal 3/25"), "{screen}");
    }

    #[test]
    fn queued_messages_show_in_the_input_title() {
        let mut app = App::new();
        app.queued_messages = vec!["next thing".into(), "after that".into()];
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let screen = (0..24)
            .map(|row| {
                (0..80)
                    .map(|column| terminal.backend().buffer()[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains("2 queued"), "{screen}");
    }

    #[test]
    fn turn_errors_offer_retry_only_with_a_known_prompt() {
        let mut app = App::new();
        app.finish_turn(Err(anyhow::anyhow!("api down")));
        assert!(!app.retry_available, "no prompt, nothing to retry");

        let mut app = App::new();
        app.last_prompt = Some("do the thing".into());
        app.finish_turn(Err(anyhow::anyhow!("api down")));
        assert!(app.retry_available);
        let (notice, _) = app.operational_notice().expect("error notice");
        assert!(notice.contains("Ctrl-R to retry"), "{notice}");

        // A fresh successful turn clears nothing prematurely.
        app.retry_available = false;
        app.finish_turn(Ok(TurnOutcome::Completed));
        assert!(!app.retry_available);
    }

    #[test]
    fn windowed_rate_measures_per_second_windows() {
        let start = std::time::Instant::now();
        let mut anchor = None;
        // First observation opens the window.
        assert_eq!(windowed_rate(&mut anchor, 1_000, start), None);
        // Within the window: no reading yet.
        assert_eq!(
            windowed_rate(
                &mut anchor,
                3_000,
                start + std::time::Duration::from_millis(500)
            ),
            None
        );
        // Window closes: (5000 - 1000) bytes over 2s = 2000 B/s.
        let rate = windowed_rate(
            &mut anchor,
            5_000,
            start + std::time::Duration::from_secs(2),
        )
        .unwrap();
        assert!((rate - 2_000.0).abs() < 1.0, "{rate}");
        // Anchor advanced: the next window measures fresh bytes.
        let rate = windowed_rate(
            &mut anchor,
            5_000,
            start + std::time::Duration::from_secs(3),
        )
        .unwrap();
        assert!(rate.abs() < 1.0, "{rate}");
    }

    #[test]
    fn thinking_status_shows_stream_liveness() {
        let mut app = App::new();
        app.push_loop_event(&LoopEvent::TurnStarted);
        // Liveness engages immediately: a pre-first-byte hang must be
        // visible, not a bare spinner.
        let plain = rendered_text(&app.status_line(120));
        assert!(plain.contains("thinking · 0 B"), "{plain}");
        app.stream_last_data = Some(std::time::Instant::now() - std::time::Duration::from_secs(12));
        let hung = rendered_text(&app.status_line(120));
        assert!(hung.contains("0 B · no data 12s"), "{hung}");
        app.stream_last_data = Some(std::time::Instant::now());

        app.push_loop_event(&LoopEvent::ThinkingDelta("x".repeat(2048)));
        let live = rendered_text(&app.status_line(120));
        assert!(live.contains("thinking · 2.0 KiB"), "{live}");
        assert!(
            !live.contains("no data"),
            "fresh data is not a stall: {live}"
        );

        // A silent stream surfaces the stall age instead of spinning forever.
        app.stream_last_data = Some(std::time::Instant::now() - std::time::Duration::from_secs(10));
        let stalled = rendered_text(&app.status_line(120));
        assert!(stalled.contains("no data 10s"), "{stalled}");

        // Responding keeps counting; narrow widths drop the counter.
        app.push_loop_event(&LoopEvent::TextDelta("y".repeat(1024)));
        let responding = rendered_text(&app.status_line(120));
        assert!(responding.contains("responding · 3.0 KiB"), "{responding}");
        let narrow = rendered_text(&app.status_line(40));
        assert!(!narrow.contains("KiB"), "{narrow}");

        // A new turn resets the counter.
        app.push_loop_event(&LoopEvent::TurnStarted);
        let reset = rendered_text(&app.status_line(120));
        assert!(!reset.contains("KiB"), "{reset}");
    }

    #[test]
    fn idle_status_keeps_model_and_latest_step_usage() {
        let mut app = App::new();
        app.configure_runtime(
            "openai/gpt-5.6-sol".into(),
            Some("high".into()),
            std::path::PathBuf::from("/workspace/project"),
            0,
            Some(272_000),
            true,
        );
        app.push_loop_event(&LoopEvent::StepComplete {
            stop_reason: "end_turn".into(),
            usage: ilar::session::Usage {
                input_tokens: 300,
                output_tokens: 50,
                cache_read_input_tokens: 1_500,
                cache_creation_input_tokens: 20,
                input_token_accounting: Some(ilar::session::InputTokenAccounting::ExcludesCached),
            },
        });
        app.push_loop_event(&LoopEvent::TurnDone {
            outcome: TurnOutcome::Completed,
        });
        app.finish_turn(Ok(TurnOutcome::Completed));

        let status = rendered_text(&app.status_line(140));
        assert!(status.contains("openai/gpt-5.6-sol@high"), "{status}");
        assert!(status.contains("in 300"), "{status}");
        assert!(status.contains("out 50"), "{status}");
        assert!(status.contains("req cache r1500/w20"), "{status}");
        assert!(status.contains("Σ 1k"), "{status}");
        assert!(status.contains("$0.004"), "{status}");
        let narrow = rendered_text(&app.status_line(60));
        assert!(narrow.contains("gpt-5.6"), "{narrow}");
        assert!(narrow.contains("high"), "{narrow}");
        assert!(narrow.contains("i300/o50"), "{narrow}");
        assert!(narrow.contains("req-cache r1k/w20"), "{narrow}");
        for width in [64, 72, 77] {
            let boundary = rendered_text(&app.status_line(width));
            assert!(boundary.contains("gpt-5.6"), "width {width}: {boundary}");
            assert!(boundary.contains("i300/o50"), "width {width}: {boundary}");
        }
        for width in 0..=120 {
            let status = rendered_text(&app.status_line(width));
            assert!(
                UnicodeWidthStr::width(status.as_str()) <= width as usize,
                "width {width}: {status:?}"
            );
        }
        for width in 0..=20 {
            let model = app.model_status_label(false, width);
            assert!(
                model.is_empty() || model.ends_with("@high"),
                "{width}: {model}"
            );
        }
        app.latest_usage = None;
        app.context_used = u64::MAX;
        app.context_limit = Some(1);
        let saturated = rendered_text(&app.status_line(64));
        assert!(
            UnicodeWidthStr::width(saturated.as_str()) <= 64,
            "{saturated}"
        );
    }

    #[test]
    fn status_line_prioritizes_notices_and_shows_a_wide_context_meter() {
        let mut app = App::new();
        app.configure_runtime(
            "openai/gpt-5.6-sol".into(),
            Some("high".into()),
            std::path::PathBuf::from("/workspace/project"),
            204_000,
            Some(272_000),
            false,
        );
        app.status = "notification paused; send a message to resume".into();
        app.set_notice(
            "notification paused; send a message to resume",
            NoticeLevel::Warning,
        );
        app.set_activity(Activity::Paused);

        let wide = rendered_text(&app.status_line(120));
        assert!(wide.contains("notification paused"), "{wide}");
        assert!(wide.contains("ctx ["), "{wide}");
        assert!(wide.contains("75%"), "{wide}");

        let narrow = rendered_text(&app.status_line(48));
        assert!(narrow.contains("notification paused"), "{narrow}");
        assert!(narrow.contains("75%"), "{narrow}");
        assert!(UnicodeWidthStr::width(narrow.as_str()) <= 48);
    }

    #[test]
    fn focused_input_border_is_stronger_and_help_moves_to_the_footer() {
        let backend = ratatui::backend::TestBackend::new(80, 12);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut app = App::new();

        terminal.draw(|frame| app.render(frame)).unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].fg, theme::BORDER);
        assert_eq!(buffer[(0, 9)].fg, theme::FOCUS_BORDER);
        assert_eq!(buffer[(0, 0)].symbol(), "┌");
        assert_eq!(buffer[(0, 9)].symbol(), "┏");
        let bottom = (0..buffer.area.width)
            .map(|x| buffer[(x, 11)].symbol())
            .collect::<String>();
        assert!(bottom.contains("Enter send"), "{bottom}");

        let backend = ratatui::backend::TestBackend::new(40, 12);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let bottom = (0..buffer.area.width)
            .map(|x| buffer[(x, 11)].symbol())
            .collect::<String>();
        assert!(bottom.contains("Enter send"), "{bottom}");
    }

    #[test]
    fn wide_tool_rows_align_state_and_nested_rows_have_tree_rails() {
        let now = std::time::Instant::now();
        let short = rendered_text(&tool_line(
            "read",
            &ToolKind::Tool,
            "src/main.rs",
            ToolState::Succeeded,
            100,
            std::time::Duration::ZERO,
            ToolProgress::None,
            now,
        ));
        let long = rendered_text(&tool_line(
            "a-much-longer-tool-name",
            &ToolKind::Tool,
            "src/main.rs",
            ToolState::Succeeded,
            100,
            std::time::Duration::ZERO,
            ToolProgress::None,
            now,
        ));
        assert_eq!(
            short.chars().position(|character| character == '✓'),
            long.chars().position(|character| character == '✓'),
            "{short:?} {long:?}"
        );

        let mut app = App::new();
        app.lines.clear();
        app.push_loop_event(&LoopEvent::ReasoningSummaryDelta("Inspecting".into()));
        app.push_loop_event(&LoopEvent::ReasoningSummaryCompleted);
        app.push_loop_event(&LoopEvent::ToolStarted {
            id: "read-1".into(),
            name: "read".into(),
        });
        app.push_loop_event(&LoopEvent::ToolFinished {
            id: "read-1".into(),
            name: "read".into(),
            is_error: false,
            result: String::new(),
            child_session_id: None,
        });
        app.toggle_transcript_target(TranscriptHitTarget::ToolGroup("live:0:read-1".into()));
        let rendered = app
            .transcript_lines(100, now)
            .iter()
            .map(rendered_text)
            .collect::<Vec<_>>();
        assert!(rendered[1].starts_with("└─tools "), "{rendered:?}");
        assert!(rendered[2].starts_with("  └─tool "), "{rendered:?}");

        let compact = app
            .transcript_lines(48, now)
            .iter()
            .map(rendered_text)
            .collect::<Vec<_>>();
        assert!(
            !compact.iter().any(|line| line.contains('─')),
            "{compact:?}"
        );
    }

    #[test]
    fn operational_notices_are_bounded_and_clear_when_work_starts() {
        let mut app = App::new();
        app.finish_turn(Err(anyhow::anyhow!("provider unavailable\nsecret detail")));

        let notice = app.notice.as_ref().expect("error notice");
        assert_eq!(notice.level, NoticeLevel::Error);
        assert_eq!(notice.text, "error: provider unavailable");
        assert!(notice.persistent);

        app.push_loop_event(&LoopEvent::TurnStarted);
        assert!(app.notice.is_some());
        app.clear_notice();
        assert!(app.notice.is_none());

        app.set_persistent_notice(
            "notification paused; send a message to resume",
            NoticeLevel::Warning,
        );
        app.clear_transient_notice();
        assert!(app.notice.is_some());
        app.set_notice("aborting turn…", NoticeLevel::Warning);
        assert_eq!(
            app.notice.as_ref().map(|notice| notice.text.as_str()),
            Some("notification paused; send a message to resume")
        );
    }

    #[test]
    fn transcript_tail_renders_beyond_u16_scroll_offsets() {
        let backend = ratatui::backend::TestBackend::new(40, 9);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.lines = (0..u16::MAX as usize + 20)
            .map(|index| Line_::System(format!("row {index}")))
            .collect();
        app.lines.push(Line_::System("true tail marker".into()));

        terminal.draw(|frame| app.render(frame)).unwrap();

        assert!(app.scroll_top > u16::MAX as usize);
        let screen = (0..terminal.backend().buffer().area.height)
            .map(|row| {
                (0..terminal.backend().buffer().area.width)
                    .map(|column| terminal.backend().buffer()[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains("true tail marker"), "{screen}");
    }

    #[test]
    fn startup_context_estimate_includes_prompt_and_tools() {
        let root = std::env::temp_dir().join(format!("ilar-tui-context-{}", new_id()));
        let store = SessionStore::new(root.clone());
        let session_id = new_id();
        drop(
            store
                .create(SessionMeta {
                    session_id: session_id.clone(),
                    parent_id: None,
                    agent: "build".into(),
                    model: "zai/glm-4.7".into(),
                    workspace: None,
                })
                .unwrap(),
        );
        let system_prompt = "system context ".repeat(100);

        let (tokens, estimated) = session_context_tokens(
            &store,
            &session_id,
            &system_prompt,
            &ToolRegistry::read_only(),
        )
        .unwrap();

        assert!(estimated);
        assert!(tokens >= system_prompt.chars().count() as u64 / 4);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn direct_resume_rejects_workspace_bound_child_sessions() {
        let meta = SessionMeta {
            session_id: new_id(),
            parent_id: Some(new_id()),
            agent: "explore".into(),
            model: "zai/glm-4.7".into(),
            workspace: Some(ilar::tools::WorkspaceLocation::shared(std::env::temp_dir())),
        };

        let error = ensure_direct_resume_allowed(Some(&meta)).unwrap_err();

        assert!(error.to_string().contains("through Task"), "{error:#}");
        assert!(ensure_direct_resume_allowed(None).is_ok());
    }

    #[test]
    fn model_change_is_adopted_only_after_persistence() {
        let root = std::env::temp_dir().join(format!("ilar-tui-model-{}", new_id()));
        let store = SessionStore::new(root.clone());
        let session_id = new_id();
        drop(
            store
                .create(SessionMeta {
                    session_id: session_id.clone(),
                    parent_id: None,
                    agent: "build".into(),
                    model: "zai/glm-4.7".into(),
                    workspace: None,
                })
                .unwrap(),
        );
        let resolver = ilar::provider::MockProvider::default();

        let writer = store.acquire_writer(&session_id).unwrap();
        assert!(
            persist_model_change(&resolver, &store, &session_id, "openai/gpt-5.2", None).is_err()
        );
        assert_eq!(
            store.load(&session_id).unwrap().effective_model(),
            "zai/glm-4.7"
        );
        drop(writer);

        persist_model_change(
            &resolver,
            &store,
            &session_id,
            "openai/gpt-5.2",
            Some("high"),
        )
        .unwrap();
        assert_eq!(
            store.load(&session_id).unwrap().effective_model(),
            "openai/gpt-5.2"
        );
        assert_eq!(
            store.load(&session_id).unwrap().effective_variant(),
            Some("high".into())
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn slash_invocations_parse_and_rewrite() {
        assert_eq!(
            parse_slash_invocation("/deploy  to staging "),
            Some(("deploy", "to staging"))
        );
        assert_eq!(parse_slash_invocation("/deploy"), Some(("deploy", "")));
        assert_eq!(parse_slash_invocation("plain prompt"), None);
        assert_eq!(parse_slash_invocation("/"), None);
        assert_eq!(parse_slash_invocation("/etc/passwd is odd"), None);
        assert_eq!(parse_slash_invocation("/ leading space"), None);

        let prompt = skill_invocation_prompt("deploy", "to staging");
        assert!(prompt.contains("`skill` tool"), "{prompt}");
        assert!(prompt.contains("\"deploy\""), "{prompt}");
        assert!(prompt.contains("to staging"), "{prompt}");
        assert!(
            !skill_invocation_prompt("deploy", "").contains("Arguments"),
            "argless invocations skip the arguments clause"
        );

        let skills = vec![
            ("deploy".to_string(), "d".to_string()),
            ("release-notes".to_string(), "r".to_string()),
        ];
        assert_eq!(close_skill_matches(&skills, "rel"), vec!["release-notes"]);
        assert_eq!(
            close_skill_matches(&skills, "zzz"),
            vec!["deploy", "release-notes"],
            "no match falls back to the full (bounded) list"
        );
    }

    #[test]
    fn skills_open_as_a_palette_submenu() {
        // The palette lists a single Skills entry, not one row per skill.
        let mut palette = CommandPalette::new(palette_items());
        assert_eq!(palette.items.len(), PALETTE_COMMANDS.len());
        palette.insert_query("skill");
        assert_eq!(
            palette.handle_key(KeyCode::Enter, false),
            CommandPaletteAction::Choose(PaletteAction::Command(PaletteCommand::Skills))
        );

        let mut app = App::new();
        app.skills = vec![("deploy".into(), "Deploy things".into())];
        activate_palette_command(
            &mut app,
            PaletteAction::Command(PaletteCommand::Skills),
            Vec::new(),
        );
        let picker = app.skill_picker.as_ref().expect("skill picker opens");
        assert_eq!(picker.skills.len(), 1);
        assert!(app.command_palette.is_none());
    }

    #[test]
    fn command_palette_opens_only_while_idle_and_switches_to_model_picker() {
        assert!(is_command_palette_shortcut(&Event::Key(KeyEvent::new(
            KeyCode::Char('p'),
            KeyModifiers::CONTROL,
        ))));
        assert!(!is_command_palette_shortcut(&Event::Key(KeyEvent::new(
            KeyCode::Char('p'),
            KeyModifiers::NONE,
        ))));

        let mut app = App::new();
        app.input = "draft prompt".into();
        app.status = "paused".into();
        app.model_key_pending = true;
        app.busy = true;

        app.open_command_palette();
        assert!(app.command_palette.is_none());
        assert_eq!(app.status, "paused");

        app.busy = false;
        app.open_command_palette();
        assert!(app.command_palette.is_some());
        assert!(!app.model_key_pending);
        assert_eq!(app.status, "paused");
        assert_eq!(app.input.text(), "draft prompt");

        activate_palette_command(
            &mut app,
            PaletteAction::Command(PaletteCommand::Model),
            ilar::model::catalog().iter().collect(),
        );
        assert!(app.command_palette.is_none());
        assert!(app.model_picker.is_some());
        assert_eq!(app.input.text(), "draft prompt");

        app.model_picker = None;
        app.current_model = "openai/gpt-5.2".into();
        app.current_variant = Some("high".into());
        app.command_palette = Some(CommandPalette::new(palette_items()));
        activate_palette_command(
            &mut app,
            PaletteAction::Command(PaletteCommand::Reasoning),
            ilar::model::catalog().iter().collect(),
        );
        assert!(app.command_palette.is_none());
        assert!(app.variant_picker.is_some());

        app.variant_picker = None;
        app.command_palette = Some(CommandPalette::new(palette_items()));
        activate_palette_command(
            &mut app,
            PaletteAction::Command(PaletteCommand::Theme),
            ilar::model::catalog().iter().collect(),
        );
        assert!(app.command_palette.is_none());
        assert!(app.theme_picker.is_some());
    }

    #[test]
    fn theme_picker_previews_navigation_and_distinguishes_commit_from_cancel() {
        let mut picker = ThemePicker::new(theme::ThemeId::Terminal);

        assert_eq!(picker.selected_theme(), theme::ThemeId::Terminal);
        assert_eq!(
            picker.handle_key(KeyCode::Down, false),
            ThemePickerAction::Preview(theme::ThemeId::Carbon)
        );
        assert_eq!(picker.active_theme, theme::ThemeId::Terminal);
        assert_eq!(
            picker.handle_key(KeyCode::Esc, false),
            ThemePickerAction::Dismiss
        );

        assert_eq!(
            picker.handle_key(KeyCode::End, false),
            ThemePickerAction::Preview(theme::ThemeId::HighContrast)
        );
        assert_eq!(
            picker.handle_key(KeyCode::Enter, false),
            ThemePickerAction::Choose(theme::ThemeId::HighContrast)
        );

        let mut app = App::new();
        app.theme_picker = Some(ThemePicker::new(theme::ThemeId::Terminal));
        apply_theme_picker_action(
            &mut app,
            ThemePickerAction::Preview(theme::ThemeId::Carbon),
            |_| unreachable!(),
        );
        assert_eq!(app.theme, theme::ThemeId::Carbon);
        apply_theme_picker_action(&mut app, ThemePickerAction::Dismiss, |_| unreachable!());
        assert_eq!(app.theme, theme::ThemeId::Terminal);
        assert!(app.theme_picker.is_none());

        app.theme_picker = Some(ThemePicker::new(theme::ThemeId::Terminal));
        app.theme = theme::ThemeId::Frost;
        let mut persisted = None;
        apply_theme_picker_action(
            &mut app,
            ThemePickerAction::Choose(theme::ThemeId::Frost),
            |theme| {
                persisted = Some(theme);
                Ok(ilar::config::ThemePersistOutcome::Saved)
            },
        );
        assert_eq!(persisted, Some(theme::ThemeId::Frost));
        assert_eq!(app.theme, theme::ThemeId::Frost);
        assert!(app.theme_picker.is_none());

        app.theme_picker = Some(ThemePicker::new(theme::ThemeId::Frost));
        apply_theme_picker_action(
            &mut app,
            ThemePickerAction::Choose(theme::ThemeId::Parchment),
            |_| {
                Ok(ilar::config::ThemePersistOutcome::DurabilityUncertain(
                    "directory sync failed".into(),
                ))
            },
        );
        assert_eq!(app.theme, theme::ThemeId::Parchment);
        assert!(app.theme_picker.is_none());
        let notice = app.notice.as_ref().unwrap();
        assert_eq!(notice.level, NoticeLevel::Warning);
        assert!(notice.text.contains("durability is uncertain"));
    }

    #[test]
    fn theme_picker_blocks_events_for_the_underlying_interface() {
        let mut app = App::new();
        assert!(!app.has_modal());

        app.theme_picker = Some(ThemePicker::new(theme::ThemeId::Terminal));

        assert!(app.has_modal());
    }

    #[test]
    fn theme_picker_renders_a_full_preview_on_narrow_terminals() {
        let backend = ratatui::backend::TestBackend::new(28, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.theme = theme::ThemeId::Carbon;
        app.theme_picker = Some(ThemePicker::new(theme::ThemeId::Terminal));

        terminal.draw(|frame| app.render(frame)).unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].bg, theme::canvas(theme::ThemeId::Carbon));
        let screen = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains("themes"), "{screen}");
        assert!(screen.contains("Carbon"), "{screen}");
        assert!(screen.contains("save"), "{screen}");
        assert!(screen.contains("undo"), "{screen}");
    }

    #[test]
    fn command_palette_renders_a_selectable_command_on_narrow_terminals() {
        let backend = ratatui::backend::TestBackend::new(30, 6);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.command_palette = Some(CommandPalette::new(palette_items()));

        terminal.draw(|frame| app.render(frame)).unwrap();

        let buffer = terminal.backend().buffer();
        let screen = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains("commands"), "{screen}");
        assert!(screen.contains("search"), "{screen}");
        assert!(screen.contains("Switch model"), "{screen}");
    }

    #[test]
    fn reasoning_variant_picker_renders_on_narrow_terminals() {
        let backend = ratatui::backend::TestBackend::new(30, 6);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.variant_picker = Some(VariantPicker::new(
            ilar::model::find("openai/gpt-5.2").unwrap(),
            Some("high"),
        ));

        terminal.draw(|frame| app.render(frame)).unwrap();

        let buffer = terminal.backend().buffer();
        let screen = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains("reasoning"), "{screen}");
        assert!(screen.contains("high"), "{screen}");
    }

    #[test]
    fn model_picker_renders_on_narrow_terminals() {
        let backend = ratatui::backend::TestBackend::new(30, 6);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.model_picker = Some(ModelPicker::new(
            ilar::model::catalog().iter().collect(),
            "openai/gpt-5.6-sol",
        ));

        terminal.draw(|frame| app.render(frame)).unwrap();

        let buffer = terminal.backend().buffer();
        let screen = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains("models"), "{screen}");
        assert!(screen.contains("search"), "{screen}");
        assert!(
            screen.contains("openai"),
            "a selectable row must remain visible: {screen}"
        );

        app.model_picker.as_mut().unwrap().error = Some("switch failed".into());
        terminal.draw(|frame| app.render(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let screen = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains("switch failed"), "{screen}");
    }

    #[test]
    fn prompt_and_picker_render_visible_cursor_positions() {
        let backend = ratatui::backend::TestBackend::new(40, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.input = "abc".into();

        terminal.draw(|frame| app.render(frame)).unwrap();
        assert_eq!(
            terminal.get_cursor_position().unwrap(),
            ratatui::layout::Position::new(4, 8)
        );

        // Typing during a turn queues the message, so the caret must
        // track it. The text has to change too: TestBackend keeps the
        // last position, so an unset cursor is otherwise indistinguishable
        // from a correctly placed one.
        app.busy = true;
        app.input = "abcdefgh".into();
        terminal.draw(|frame| app.render(frame)).unwrap();
        assert_eq!(
            terminal.get_cursor_position().unwrap(),
            ratatui::layout::Position::new(9, 8),
            "the caret stopped tracking the input while a turn was running"
        );
        app.busy = false;
        app.input = "abc".into();

        let mut picker = ModelPicker::new(
            ilar::model::catalog().iter().collect(),
            "openai/gpt-5.6-sol",
        );
        picker.set_query("glm");
        app.model_picker = Some(picker);
        terminal.draw(|frame| app.render(frame)).unwrap();
        assert_eq!(
            terminal.get_cursor_position().unwrap(),
            ratatui::layout::Position::new(12, 2)
        );
    }

    /// Ctrl-R must not clobber a draft: an unsubmitted draft is not in
    /// the history, so overwriting it loses the text for good.
    #[test]
    fn retry_declines_rather_than_discarding_an_unsent_draft() {
        let mut app = App::new();
        app.last_prompt = Some("previous prompt".into());
        app.retry_available = true;
        app.input = "half-written thought".into();

        assert!(begin_retry(&mut app).is_none());
        assert_eq!(app.input.text(), "half-written thought");
        assert!(app.retry_available, "retry stays on offer");

        app.input.clear();
        assert!(begin_retry(&mut app).is_some());
        assert_eq!(app.input.text(), "previous prompt");
        assert!(!app.retry_available);
    }

    #[test]
    fn wrapped_assistant_lines_keep_content_indent() {
        let backend = ratatui::backend::TestBackend::new(32, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.lines = vec![Line_::Assistant(
            "abcdefghijklmnopqrstuvwxyz0123456789".into(),
        )];

        terminal.draw(|frame| app.render(frame)).unwrap();

        let buffer = terminal.backend().buffer();
        let continuation_start = (1..buffer.area.width.saturating_sub(1))
            .find(|x| buffer[(*x, 2)].symbol() != " ")
            .unwrap();
        assert_eq!(continuation_start, 8);
    }

    #[test]
    fn markdown_separator_occupies_one_final_terminal_row() {
        let backend = ratatui::backend::TestBackend::new(48, 12);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.lines = vec![Line_::Assistant(
            "- final list item\n\n## Section\n\nParagraph text.".into(),
        )];

        terminal.draw(|frame| app.render(frame)).unwrap();

        let rows = (0..terminal.backend().buffer().area.height)
            .map(|y| {
                (0..terminal.backend().buffer().area.width)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        let list = rows
            .iter()
            .position(|row| row.contains("final list item"))
            .unwrap();
        let heading = rows.iter().position(|row| row.contains("Section")).unwrap();
        let paragraph = rows
            .iter()
            .position(|row| row.contains("Paragraph text"))
            .unwrap();
        assert_eq!(heading - list, 2, "rows: {rows:#?}");
        assert_eq!(paragraph - heading, 2, "rows: {rows:#?}");
    }

    #[test]
    fn turn_error_is_visible() {
        let mut app = App::new();
        app.busy = true;

        app.finish_turn(Err(anyhow::anyhow!("provider rejected tool result")));

        assert!(!app.busy);
        assert_eq!(app.status, "error");
        assert!(matches!(
            app.lines.last(),
            Some(Line_::System(message)) if message.contains("provider rejected tool result")
        ));
    }

    #[test]
    fn turn_done_keeps_ownership_until_join_cleanup() {
        let mut app = App::new();
        app.busy = true;

        app.push_loop_event(&LoopEvent::TurnDone {
            outcome: TurnOutcome::Completed,
        });
        assert!(app.busy);

        app.finish_turn(Ok(TurnOutcome::Completed));
        assert!(!app.busy);
    }

    #[test]
    fn terminal_bell_waits_for_idle_and_only_writes_once() {
        let mut output = Vec::new();
        let mut pending = true;

        assert!(!ring_terminal_bell_if_idle(&mut output, &mut pending, true).unwrap());
        assert!(output.is_empty());
        assert!(pending);

        assert!(ring_terminal_bell_if_idle(&mut output, &mut pending, false).unwrap());
        assert_eq!(output, b"\x07");
        assert!(!pending);

        assert!(!ring_terminal_bell_if_idle(&mut output, &mut pending, false).unwrap());
        assert_eq!(output, b"\x07");
    }

    #[test]
    fn completed_consecutive_tools_collapse_to_one_group_row() {
        let mut app = App::new();
        app.lines.clear();
        for (id, name) in [("call-1", "read"), ("call-2", "grep")] {
            app.push_loop_event(&LoopEvent::ToolStarted {
                id: id.into(),
                name: name.into(),
            });
            app.push_loop_event(&LoopEvent::ToolFinished {
                id: id.into(),
                name: name.into(),
                is_error: false,
                result: String::new(),
                child_session_id: None,
            });
        }

        let rendered = app
            .transcript_lines(80, std::time::Instant::now())
            .iter()
            .map(rendered_text)
            .collect::<Vec<_>>();

        assert_eq!(rendered.len(), 1);
        assert!(rendered[0].contains("tools"), "{rendered:?}");
        assert!(rendered[0].contains("2 calls"), "{rendered:?}");

        app.toggle_transcript_target(TranscriptHitTarget::ToolGroup("live:0:call-1".into()));
        let expanded = app
            .transcript_lines(80, std::time::Instant::now())
            .iter()
            .map(rendered_text)
            .collect::<Vec<_>>();
        assert_eq!(expanded.len(), 3);
        assert!(expanded.iter().any(|line| line.contains("read")));
        assert!(expanded.iter().any(|line| line.contains("grep")));
    }

    #[test]
    fn provider_steps_in_one_thought_phase_share_a_group() {
        let mut app = App::new();
        app.lines.clear();
        for (index, (id, name)) in [("call-1", "read"), ("call-2", "grep")]
            .into_iter()
            .enumerate()
        {
            app.push_loop_event(&LoopEvent::ToolStarted {
                id: id.into(),
                name: name.into(),
            });
            app.push_loop_event(&LoopEvent::ToolFinished {
                id: id.into(),
                name: name.into(),
                is_error: false,
                result: String::new(),
                child_session_id: None,
            });
            if index == 0 {
                app.push_loop_event(&LoopEvent::StepComplete {
                    stop_reason: "tool_use".into(),
                    usage: Default::default(),
                });
            }
        }

        let rendered = app
            .transcript_lines(80, std::time::Instant::now())
            .iter()
            .map(rendered_text)
            .collect::<Vec<_>>();
        assert_eq!(rendered.len(), 1);
        assert!(rendered[0].contains("tools"), "{rendered:?}");
        assert!(rendered[0].contains("2 calls"), "{rendered:?}");
    }

    #[test]
    fn single_tool_group_is_a_compact_child_of_its_thought() {
        let mut app = App::new();
        app.lines.clear();
        app.push_loop_event(&LoopEvent::ReasoningSummaryDelta(
            "Inspecting layout".into(),
        ));
        app.push_loop_event(&LoopEvent::ReasoningSummaryCompleted);
        app.push_loop_event(&LoopEvent::ToolStarted {
            id: "read-1".into(),
            name: "read".into(),
        });
        app.push_loop_event(&LoopEvent::ToolFinished {
            id: "read-1".into(),
            name: "read".into(),
            is_error: false,
            result: String::new(),
            child_session_id: None,
        });

        let rendered = app
            .transcript_lines(80, std::time::Instant::now())
            .iter()
            .map(rendered_text)
            .collect::<Vec<_>>();

        assert_eq!(rendered.len(), 2);
        assert!(rendered[0].contains("Thought: Inspecting layout"));
        assert!(rendered[1].starts_with("└─tools "), "{rendered:?}");
        assert!(rendered[1].contains("1 call"), "{rendered:?}");
        for width in 0..=2 {
            app.transcript_cache.update(
                &app.lines,
                &app.expanded_tool_groups,
                app.transcript_revision,
                width,
                std::time::Instant::now(),
                app.activity_started,
            );
            assert!(
                app.transcript_cache
                    .visible_rows(0, usize::MAX, &[])
                    .iter()
                    .all(|row| row.line.width() <= width as usize),
                "width {width}"
            );
        }
    }

    #[test]
    fn agent_is_a_spaced_top_level_parent_outside_tool_groups() {
        let mut app = App::new();
        app.lines.clear();
        app.push_loop_event(&LoopEvent::ToolStarted {
            id: "read-1".into(),
            name: "read".into(),
        });
        app.push_loop_event(&LoopEvent::ToolStarted {
            id: "task-1".into(),
            name: "task".into(),
        });
        app.push_loop_event(&LoopEvent::SubagentConfigured {
            id: "task-1".into(),
            description: "Inspect rendering".into(),
            agent: "explore".into(),
            model: None,
        });

        let rendered = app
            .transcript_lines(100, std::time::Instant::now())
            .iter()
            .map(rendered_text)
            .collect::<Vec<_>>();

        assert!(rendered[0].starts_with("tools "), "{rendered:?}");
        assert!(rendered[1].starts_with("└─tool "), "{rendered:?}");
        assert_eq!(rendered[2], "");
        assert!(rendered[3].starts_with("agent "), "{rendered:?}");
        assert!(rendered[4].contains("thinking"), "{rendered:?}");
    }

    #[test]
    fn tool_runs_around_an_agent_expand_independently() {
        let mut app = App::new();
        app.lines.clear();
        for (id, name) in [("read-1", "read"), ("task-1", "task"), ("grep-1", "grep")] {
            app.push_loop_event(&LoopEvent::ToolStarted {
                id: id.into(),
                name: name.into(),
            });
            if name == "task" {
                app.push_loop_event(&LoopEvent::SubagentConfigured {
                    id: id.into(),
                    description: "Inspect".into(),
                    agent: "explore".into(),
                    model: None,
                });
            }
            app.push_loop_event(&LoopEvent::ToolFinished {
                id: id.into(),
                name: name.into(),
                is_error: false,
                result: String::new(),
                child_session_id: None,
            });
        }

        app.toggle_transcript_target(TranscriptHitTarget::ToolGroup("live:0:read-1".into()));
        let rendered = app
            .transcript_lines(100, std::time::Instant::now())
            .iter()
            .map(rendered_text)
            .collect::<Vec<_>>();

        assert!(rendered.iter().any(|line| line.contains("read")));
        assert!(!rendered.iter().any(|line| line.contains("grep")));
        assert_eq!(
            rendered
                .iter()
                .filter(|line| line.contains("tools ▸"))
                .count(),
            1
        );
    }

    #[test]
    fn collapsed_running_group_shows_only_active_children() {
        let mut app = App::new();
        app.lines.clear();
        for (id, name) in [("call-1", "read"), ("call-2", "grep")] {
            app.push_loop_event(&LoopEvent::ToolStarted {
                id: id.into(),
                name: name.into(),
            });
        }
        app.push_loop_event(&LoopEvent::ToolFinished {
            id: "call-1".into(),
            name: "read".into(),
            is_error: false,
            result: String::new(),
            child_session_id: None,
        });

        let rendered = app
            .transcript_lines(100, std::time::Instant::now())
            .iter()
            .map(rendered_text)
            .collect::<Vec<_>>();

        assert!(rendered[0].contains("1 running · 2 calls"), "{rendered:?}");
        assert!(rendered.iter().any(|line| line.contains("grep")));
        assert!(!rendered.iter().any(|line| line.contains("read")));
    }

    #[test]
    fn top_level_items_have_one_blank_separator_row() {
        let mut app = App::new();
        app.lines = vec![
            Line_::User("Question".into()),
            Line_::Thought {
                id: String::new(),
                text: "Answering".into(),
                complete: true,
                expanded: false,
            },
            Line_::Assistant("Response".into()),
        ];

        let rendered = app
            .transcript_lines(80, std::time::Instant::now())
            .iter()
            .map(rendered_text)
            .collect::<Vec<_>>();

        assert_eq!(
            rendered,
            [
                "you  Question",
                "",
                "+ Thought: Answering",
                "",
                "ilar Response"
            ]
        );
    }

    #[test]
    fn expanded_tool_shows_bounded_arguments_and_result() {
        let mut app = App::new();
        app.lines.clear();
        app.push_loop_event(&LoopEvent::ToolStarted {
            id: "read-1".into(),
            name: "read".into(),
        });
        app.push_loop_event(&LoopEvent::ToolInputComplete {
            id: "read-1".into(),
            arguments: "{\n  \"path\": \"src/main.rs\"\n}".into(),
        });
        app.push_loop_event(&LoopEvent::ToolFinished {
            id: "read-1".into(),
            name: "read".into(),
            is_error: false,
            result: (1..=12)
                .map(|line| format!("result line {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
            child_session_id: None,
        });
        app.toggle_transcript_target(TranscriptHitTarget::ToolGroup("live:0:read-1".into()));
        app.toggle_transcript_target(TranscriptHitTarget::Tool("read-1".into()));

        let rendered = app
            .transcript_lines(60, std::time::Instant::now())
            .iter()
            .map(rendered_text)
            .collect::<Vec<_>>();

        assert!(rendered.iter().any(|line| line.contains("args")));
        assert!(rendered.iter().any(|line| line.contains("src/main.rs")));
        assert!(rendered.iter().any(|line| line.contains("result")));
        assert!(rendered.iter().any(|line| line.contains("… more")));
        assert!(!rendered.iter().any(|line| line.contains("result line 12")));
        assert!(rendered.iter().all(|line| line.width() <= 60));
        for width in 0..=20 {
            let narrow = app.transcript_lines(width, std::time::Instant::now());
            assert!(
                narrow.iter().all(|line| line.width() <= width as usize),
                "width {width}: {:?}",
                narrow.iter().map(rendered_text).collect::<Vec<_>>()
            );
        }

        app.toggle_transcript_target(TranscriptHitTarget::Tool("read-1".into()));
        let full = app
            .transcript_lines(60, std::time::Instant::now())
            .iter()
            .map(rendered_text)
            .collect::<Vec<_>>();
        assert!(full.iter().any(|line| line.contains("result line 12")));
        assert!(!full.iter().any(|line| line.contains("… more")));

        app.toggle_transcript_target(TranscriptHitTarget::Tool("read-1".into()));
        let collapsed = app
            .transcript_lines(60, std::time::Instant::now())
            .iter()
            .map(rendered_text)
            .collect::<Vec<_>>();
        assert_eq!(collapsed.len(), 2);
        assert!(!collapsed.iter().any(|line| line.contains("args")));
    }

    #[test]
    fn expanded_edit_tool_shows_colored_diff_instead_of_args() {
        let mut app = App::new();
        app.lines.clear();
        app.push_loop_event(&LoopEvent::ToolStarted {
            id: "edit-1".into(),
            name: "edit".into(),
        });
        app.push_loop_event(&LoopEvent::ToolInputComplete {
            id: "edit-1".into(),
            arguments: serde_json::json!({
                "path": "src/lib.rs",
                "old_string": "shared\nbefore\nshared tail",
                "new_string": "shared\nafter\nshared tail",
            })
            .to_string(),
        });
        app.push_loop_event(&LoopEvent::ToolFinished {
            id: "edit-1".into(),
            name: "edit".into(),
            is_error: false,
            result: "edited src/lib.rs: 1 replacement".into(),
            child_session_id: None,
        });
        app.toggle_transcript_target(TranscriptHitTarget::ToolGroup("live:0:edit-1".into()));
        app.toggle_transcript_target(TranscriptHitTarget::Tool("edit-1".into()));

        let lines = app.transcript_lines(60, std::time::Instant::now());
        let rendered = lines.iter().map(rendered_text).collect::<Vec<_>>();
        assert!(
            rendered.iter().any(|line| line.contains("diff")),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|line| line.contains("- before")),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|line| line.contains("+ after")),
            "{rendered:?}"
        );
        assert!(
            !rendered.iter().any(|line| line.contains("old_string")),
            "raw args JSON must be replaced by the diff: {rendered:?}"
        );
        let colors: Vec<_> = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter_map(|span| span.style.fg.map(|color| (span.content.to_string(), color)))
            .collect();
        assert!(
            colors
                .iter()
                .any(|(text, color)| text.contains("+ after") && *color == theme::SUCCESS),
            "{colors:?}"
        );
        assert!(
            colors
                .iter()
                .any(|(text, color)| text.contains("- before") && *color == ERROR),
            "{colors:?}"
        );

        for width in 0..=20 {
            let narrow = app.transcript_lines(width, std::time::Instant::now());
            assert!(
                narrow.iter().all(|line| line.width() <= width as usize),
                "width {width}"
            );
        }
    }

    #[test]
    fn agent_shows_live_children_and_expands_completed_timeline() {
        let mut app = App::new();
        app.lines.clear();
        app.push_loop_event(&LoopEvent::ToolStarted {
            id: "task-1".into(),
            name: "task".into(),
        });
        app.push_loop_event(&LoopEvent::SubagentConfigured {
            id: "task-1".into(),
            description: "Inspect rendering".into(),
            agent: "explore".into(),
            model: None,
        });
        for event in [
            LoopEvent::ReasoningSummaryDelta("Tracing transcript".into()),
            LoopEvent::ReasoningSummaryCompleted,
            LoopEvent::ToolStarted {
                id: "child-read".into(),
                name: "read".into(),
            },
        ] {
            app.push_subagent_activity(&ilar::subagent::SubagentActivity {
                parent_session_id: String::new(),
                parent_call_id: "task-1".into(),
                child_session_id: "child-session".into(),
                event,
            });
        }

        let live = app
            .transcript_lines(100, std::time::Instant::now())
            .iter()
            .map(rendered_text)
            .collect::<Vec<_>>();
        assert!(
            live.iter()
                .any(|line| line.contains("Thought: Tracing transcript"))
        );
        assert!(
            live.iter()
                .any(|line| line.contains("child-read") || line.contains("read"))
        );
        assert!(
            live.iter()
                .all(|line| !line.contains("└─└─") && !line.contains("├─└─")),
            "{live:?}"
        );

        app.push_subagent_activity(&ilar::subagent::SubagentActivity {
            parent_session_id: String::new(),
            parent_call_id: "task-1".into(),
            child_session_id: "child-session".into(),
            event: LoopEvent::TextDelta("Nested answer".into()),
        });
        app.push_subagent_activity(&ilar::subagent::SubagentActivity {
            parent_session_id: String::new(),
            parent_call_id: "task-1".into(),
            child_session_id: "child-session".into(),
            event: LoopEvent::TurnDone {
                outcome: TurnOutcome::Completed,
            },
        });
        app.push_loop_event(&LoopEvent::ToolFinished {
            id: "task-1".into(),
            name: "task".into(),
            is_error: false,
            result: "Nested answer".into(),
            child_session_id: Some("child-session".into()),
        });
        let collapsed = app
            .transcript_lines(100, std::time::Instant::now())
            .iter()
            .map(rendered_text)
            .collect::<Vec<_>>();
        assert!(!collapsed.iter().any(|line| line.contains("Nested answer")));

        app.toggle_transcript_target(TranscriptHitTarget::Tool("task-1".into()));
        let expanded = app
            .transcript_lines(100, std::time::Instant::now())
            .iter()
            .map(rendered_text)
            .collect::<Vec<_>>();
        assert!(expanded.iter().any(|line| line.contains("Nested answer")));
        assert!(
            expanded
                .iter()
                .any(|line| line.contains("Tracing transcript"))
        );
    }

    #[test]
    fn early_child_activity_waits_for_its_parent_row() {
        let mut app = App::new();
        app.lines.clear();
        app.push_subagent_activity(&ilar::subagent::SubagentActivity {
            parent_session_id: "root-session".into(),
            parent_call_id: "task-early".into(),
            child_session_id: "child-early".into(),
            event: LoopEvent::ReasoningSummaryDelta("Already working".into()),
        });
        assert_eq!(app.pending_subagent_activity.len(), 1);

        app.push_loop_event(&LoopEvent::ToolStarted {
            id: "task-early".into(),
            name: "task".into(),
        });
        app.push_loop_event(&LoopEvent::SubagentConfigured {
            id: "task-early".into(),
            description: "Fast child".into(),
            agent: "explore".into(),
            model: None,
        });
        app.retry_subagent_activity();

        assert!(app.pending_subagent_activity.is_empty());
        assert!(matches!(
            app.lines.last(),
            Some(Line_::Tool { child_lines, .. })
                if matches!(child_lines.last(), Some(Line_::Thought { text, .. }) if text == "Already working")
        ));
    }

    #[test]
    fn zero_distance_click_toggles_a_semantic_transcript_row() {
        let mut app = App::new();
        app.transcript_text_area = Rect::new(4, 2, 40, 1);
        app.transcript_hit_targets = vec![Some(TranscriptHitTarget::ToolGroup("group-1".into()))];

        app.begin_transcript_selection(5, 2);
        assert_eq!(app.finish_transcript_selection(5, 2), None);

        assert!(app.expanded_tool_groups.contains("group-1"));
        assert!(app.transcript_selection.is_none());
    }

    #[test]
    fn aborted_turn_closes_running_tool_rows() {
        let mut app = App::new();
        app.push_loop_event(&LoopEvent::ToolStarted {
            id: "call-1".into(),
            name: "bash".into(),
        });

        app.push_loop_event(&LoopEvent::TurnDone {
            outcome: TurnOutcome::Aborted,
        });

        assert!(matches!(
            app.lines.last(),
            Some(Line_::Tool {
                state: ToolState::Failed,
                ..
            })
        ));
    }

    #[test]
    fn parallel_tool_completions_match_by_id() {
        let mut app = App::new();
        app.push_loop_event(&LoopEvent::ToolStarted {
            id: "read-1".into(),
            name: "read".into(),
        });
        app.push_loop_event(&LoopEvent::ToolStarted {
            id: "todo-1".into(),
            name: "todo".into(),
        });
        app.push_loop_event(&LoopEvent::ToolFinished {
            id: "read-1".into(),
            name: "read".into(),
            is_error: false,
            result: String::new(),
            child_session_id: None,
        });

        assert!(matches!(
            &app.lines[1],
            Line_::Tool { id, state: ToolState::Succeeded, .. } if id == "read-1"
        ));
        assert!(matches!(
            &app.lines[2],
            Line_::Tool { id, state: ToolState::Running, .. } if id == "todo-1"
        ));
    }

    #[test]
    fn tool_arguments_are_muted_id_safe_and_single_line() {
        let mut app = App::new();
        app.push_loop_event(&LoopEvent::ToolStarted {
            id: "bash-1".into(),
            name: "bash".into(),
        });
        app.push_loop_event(&LoopEvent::ToolArguments {
            id: "bash-1".into(),
            arguments: "cargo test --workspace && cargo clippy --workspace".into(),
        });
        app.push_loop_event(&LoopEvent::ToolFinished {
            id: "bash-1".into(),
            name: "bash".into(),
            is_error: false,
            result: String::new(),
            child_session_id: None,
        });
        app.toggle_transcript_target(TranscriptHitTarget::ToolGroup("live:0:bash-1".into()));

        let lines = app.transcript_lines(36, std::time::Instant::now());
        let tool = lines.last().unwrap();
        assert!(UnicodeWidthStr::width(rendered_text(tool).as_str()) <= 36);
        assert_eq!(tool.spans.last().unwrap().style.fg, Some(theme::SECONDARY));
        assert!(rendered_text(tool).contains("cargo test"));
        assert!(!rendered_text(tool).contains('\n'));
    }

    #[test]
    fn write_progress_distinguishes_receiving_waiting_and_writing() {
        let now = std::time::Instant::now();
        let receiving = tool_line(
            "write",
            &ToolKind::Tool,
            "src/generated.html",
            ToolState::Running,
            120,
            std::time::Duration::ZERO,
            ToolProgress::Receiving {
                received_bytes: 48 * 1024,
                last_data: now,
            },
            now,
        );
        let receiving = rendered_text(&receiving);
        assert!(receiving.contains("src/generated.html"), "{receiving}");
        assert!(receiving.contains("receiving 48.0 KiB"), "{receiving}");

        let waiting = tool_line(
            "write",
            &ToolKind::Tool,
            "src/generated.html",
            ToolState::Running,
            120,
            std::time::Duration::ZERO,
            ToolProgress::Receiving {
                received_bytes: 48 * 1024,
                last_data: now - std::time::Duration::from_secs(3),
            },
            now,
        );
        let waiting = rendered_text(&waiting);
        assert!(waiting.contains("waiting for provider"), "{waiting}");
        assert!(waiting.contains("last data 3s ago"), "{waiting}");

        let mut app = App::new();
        app.push_loop_event(&LoopEvent::ToolStarted {
            id: "write-1".into(),
            name: "write".into(),
        });
        app.push_loop_event(&LoopEvent::ToolInputProgress {
            id: "write-1".into(),
            received_bytes: 48 * 1024,
            last_data: now,
        });
        app.push_loop_event(&LoopEvent::ToolInputComplete {
            id: "write-1".into(),
            arguments: "{\"path\":\"src/generated.html\"}".into(),
        });
        let queued = app.transcript_lines(120, now);
        let queued = rendered_text(queued.last().unwrap());
        assert!(queued.contains("queued"), "{queued}");
        assert!(!queued.contains("provider"), "{queued}");

        app.push_loop_event(&LoopEvent::ToolInputProgress {
            id: "write-1".into(),
            received_bytes: 48 * 1024,
            last_data: now,
        });
        let still_queued = app.transcript_lines(120, now);
        assert!(
            rendered_text(still_queued.last().unwrap()).contains("queued"),
            "{still_queued:?}"
        );
        let long_queued = rendered_text(&tool_line(
            "bash",
            &ToolKind::Tool,
            "git status --short && find . -maxdepth 3 -type f | sort | sed -n 1,200p",
            ToolState::Running,
            36,
            std::time::Duration::ZERO,
            ToolProgress::Queued,
            now,
        ));
        assert!(long_queued.contains("queued"), "{long_queued}");
        // Explicit model overrides render as agent@model (short id).
        let pinned = rendered_text(&tool_line(
            "task",
            &ToolKind::Agent {
                name: "explore".into(),
                model: Some("zai/glm-5.3".into()),
            },
            "grep the tree",
            ToolState::Running,
            120,
            std::time::Duration::ZERO,
            ToolProgress::None,
            std::time::Instant::now(),
        ));
        assert!(pinned.contains("explore@glm-5.3"), "{pinned}");

        let narrow_agent = rendered_text(&tool_line(
            "task",
            &ToolKind::Agent {
                name: "repository-reviewer".into(),
                model: None,
            },
            "inspect every lifecycle path",
            ToolState::Running,
            26,
            std::time::Duration::ZERO,
            ToolProgress::Queued,
            now,
        ));
        assert!(narrow_agent.contains("queued"), "{narrow_agent}");

        app.push_loop_event(&LoopEvent::ToolExecutionStarted {
            id: "write-1".into(),
            received_bytes: 64 * 1024,
            started: now,
        });
        let writing = app.transcript_lines(120, now);
        let writing = rendered_text(writing.last().unwrap());
        assert!(writing.contains("writing 64.0 KiB · 0s"), "{writing}");
        assert!(!writing.contains("received"), "{writing}");
        app.push_loop_event(&LoopEvent::ToolExecutionCompleted {
            id: "write-1".into(),
        });
        let complete = rendered_text(app.transcript_lines(120, now).last().unwrap());
        assert!(complete.contains("done"), "{complete}");

        let executing = tool_line(
            "bash",
            &ToolKind::Tool,
            "cargo test",
            ToolState::Running,
            120,
            std::time::Duration::ZERO,
            ToolProgress::Executing {
                received_bytes: 2048,
                started: now - std::time::Duration::from_secs(3),
            },
            now,
        );
        let executing = rendered_text(&executing);
        assert!(executing.contains("executing · 3s"), "{executing}");
        assert!(!executing.contains("received"), "{executing}");

        let mut agent_app = App::new();
        agent_app.push_loop_event(&LoopEvent::ToolStarted {
            id: "task-1".into(),
            name: "task".into(),
        });
        agent_app.push_loop_event(&LoopEvent::ToolArguments {
            id: "task-1".into(),
            arguments: "ambiguous · summary".into(),
        });
        agent_app.push_loop_event(&LoopEvent::SubagentConfigured {
            id: "task-1".into(),
            description: "Review security paths".into(),
            agent: "build · secure".into(),
            model: None,
        });
        agent_app.push_loop_event(&LoopEvent::ToolInputComplete {
            id: "task-1".into(),
            arguments: "{\"description\":\"Review security paths\"}".into(),
        });
        agent_app.push_loop_event(&LoopEvent::ToolExecutionStarted {
            id: "task-1".into(),
            received_bytes: 406,
            started: now - std::time::Duration::from_secs(72),
        });
        let subagent = agent_app
            .transcript_lines(120, now)
            .iter()
            .map(rendered_text)
            .find(|line| line.contains("agent"))
            .unwrap();
        assert!(subagent.contains("agent ▶ build · secure"), "{subagent}");
        assert!(subagent.contains("Review security paths"), "{subagent}");
        assert!(subagent.contains("running · 1m 12s"), "{subagent}");
        assert!(!subagent.contains("received"), "{subagent}");
    }

    #[test]
    fn telemetry_always_contains_runtime_context() {
        let mut app = App::new();
        app.configure_runtime(
            "openai/gpt-5.6-sol".into(),
            None,
            std::path::PathBuf::from("/very/long/workspace/project"),
            68_000,
            Some(272_000),
            false,
        );
        let wide = rendered_text(&app.status_line(100));
        assert!(wide.contains("ready"));
        assert!(wide.contains("openai/gpt-5.6-sol"));
        assert!(wide.contains("project"));
        assert!(wide.contains("ctx ["), "{wide}");
        assert!(wide.contains("25%"));

        let narrow = rendered_text(&app.status_line(40));
        assert!(UnicodeWidthStr::width(narrow.as_str()) <= 40, "{narrow}");
        assert!(narrow.contains("ready"));
        assert!(narrow.contains("25%"));

        let boundary = rendered_text(&app.status_line(64));
        assert!(
            UnicodeWidthStr::width(boundary.as_str()) <= 64,
            "{boundary}"
        );
        assert!(boundary.contains("25%"), "{boundary}");

        for width in 0..=100 {
            let line = rendered_text(&app.status_line(width));
            assert!(
                UnicodeWidthStr::width(line.as_str()) <= width as usize,
                "width {width}: {line:?}"
            );
        }
    }

    #[test]
    fn telemetry_counts_uncached_and_cached_context_categories() {
        let mut app = App::new();
        app.push_loop_event(&LoopEvent::StepComplete {
            stop_reason: "end_turn".into(),
            usage: ilar::session::Usage {
                input_tokens: 300,
                output_tokens: 50,
                cache_read_input_tokens: 1_500,
                cache_creation_input_tokens: 0,
                input_token_accounting: Some(ilar::session::InputTokenAccounting::ExcludesCached),
            },
        });
        assert_eq!(app.context_used, 1_850);
        assert!(!app.context_estimated);
    }

    #[test]
    fn activity_row_animates_and_clears_with_turn() {
        let mut app = App::new();
        app.busy = true;
        app.push_loop_event(&LoopEvent::TurnStarted);
        let thinking = app.transcript_lines(80, app.activity_started);
        assert!(rendered_text(thinking.last().unwrap()).contains("thinking"));
        let next_frame = app.transcript_lines(
            80,
            app.activity_started + std::time::Duration::from_millis(200),
        );
        assert_ne!(
            rendered_text(thinking.last().unwrap()),
            rendered_text(next_frame.last().unwrap())
        );

        app.push_loop_event(&LoopEvent::ToolStarted {
            id: "tool-1".into(),
            name: "read".into(),
        });
        let tools = app.transcript_lines(80, app.activity_started);
        assert!(rendered_text(tools.last().unwrap()).contains("processing tools and agents"));
        let tool_now = std::time::Instant::now();
        let first_tool = tool_line(
            "read",
            &ToolKind::Tool,
            "src/main.rs",
            ToolState::Running,
            80,
            std::time::Duration::ZERO,
            ToolProgress::None,
            tool_now,
        );
        let next_tool = tool_line(
            "read",
            &ToolKind::Tool,
            "src/main.rs",
            ToolState::Running,
            80,
            std::time::Duration::from_millis(200),
            ToolProgress::None,
            tool_now + std::time::Duration::from_millis(200),
        );
        assert_ne!(rendered_text(&first_tool), rendered_text(&next_tool));

        app.push_loop_event(&LoopEvent::TextDelta("hello".into()));
        let responding = app.transcript_lines(80, app.activity_started);
        assert!(rendered_text(responding.last().unwrap()).contains("responding"));

        app.push_loop_event(&LoopEvent::TurnDone {
            outcome: TurnOutcome::Completed,
        });
        app.finish_turn(Ok(TurnOutcome::Completed));
        let complete = app.transcript_lines(80, std::time::Instant::now());
        assert!(!rendered_text(complete.last().unwrap()).contains("responding"));
    }

    #[test]
    fn raw_thinking_accumulates_and_expands_on_click() {
        let mut app = App::new();
        app.lines.clear();
        app.push_loop_event(&LoopEvent::TurnStarted);
        app.push_loop_event(&LoopEvent::ThinkingDelta(
            "First I will check the parser.\nNow comparing".into(),
        ));
        app.push_loop_event(&LoopEvent::ThinkingDelta(" the two branches.".into()));

        // Collapsed: tail-first title shows what it is doing right now.
        let live = app.transcript_lines(100, std::time::Instant::now());
        let rendered: Vec<String> = live.iter().map(rendered_text).collect();
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("▸ Thinking: Now comparing the two branches.")),
            "{rendered:?}"
        );
        assert!(
            !rendered
                .iter()
                .any(|line| line.contains("check the parser"))
        );

        // Click expands to the full (bounded) thinking text. Targets are
        // computed during render, so draw once first.
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let target = app
            .transcript_hit_targets
            .iter()
            .flatten()
            .find(|target| matches!(target, TranscriptHitTarget::Thought(_)))
            .cloned();
        let Some(target) = target else {
            panic!(
                "thought row must be clickable: {:?}",
                app.transcript_hit_targets
            )
        };
        app.toggle_transcript_target(target.clone());
        let expanded = app.transcript_lines(100, std::time::Instant::now());
        let rendered: Vec<String> = expanded.iter().map(rendered_text).collect();
        assert!(
            rendered.iter().any(|line| line.contains("▾ Thinking:")),
            "{rendered:?}"
        );
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("First I will check the parser.")),
            "{rendered:?}"
        );
        app.toggle_transcript_target(target);
        let collapsed = app.transcript_lines(100, std::time::Instant::now());
        assert!(
            !collapsed
                .iter()
                .map(rendered_text)
                .any(|line| line.contains("check the parser")),
        );

        // Text starting: the thought completes and stays in the transcript.
        app.push_loop_event(&LoopEvent::TextDelta("The answer".into()));
        assert!(
            app.lines
                .iter()
                .any(|line| matches!(line, Line_::Thought { complete: true, .. }))
        );
        app.push_loop_event(&LoopEvent::TurnDone {
            outcome: TurnOutcome::Completed,
        });
        assert!(
            app.lines
                .iter()
                .any(|line| matches!(line, Line_::Thought { complete: true, .. }))
        );
    }

    #[test]
    fn streamed_reasoning_summary_becomes_a_completed_thought_row() {
        assert_eq!(
            reasoning_summary_title("## Reviewing tests ##\n\nMore detail"),
            "Reviewing tests"
        );
        let mut app = App::new();
        app.push_loop_event(&LoopEvent::ReasoningSummaryDelta("**Running".into()));
        app.push_loop_event(&LoopEvent::ReasoningSummaryDelta(
            " tests**\n\nChecking the suite.".into(),
        ));
        let live = app.transcript_lines(80, std::time::Instant::now());
        assert!(
            rendered_text(live.last().unwrap()).contains("Thinking: Running tests"),
            "{live:?}"
        );

        app.push_loop_event(&LoopEvent::ReasoningSummaryCompleted);
        let complete = app.transcript_lines(80, std::time::Instant::now());
        assert!(
            rendered_text(complete.last().unwrap()).contains("Thought: Running tests"),
            "{complete:?}"
        );
        assert!(!rendered_text(complete.last().unwrap()).contains("Checking the suite"));

        let mut interrupted = App::new();
        interrupted.push_loop_event(&LoopEvent::ReasoningSummaryDelta("**Partial".into()));
        interrupted.finish_turn(Err(anyhow::anyhow!("connection lost")));
        assert!(
            !interrupted
                .lines
                .iter()
                .any(|line| matches!(line, Line_::Thought { .. }))
        );
    }

    #[test]
    fn stopped_and_paused_states_remain_visible() {
        let mut app = App::new();
        app.push_loop_event(&LoopEvent::TurnDone {
            outcome: TurnOutcome::MaxIterations,
        });
        assert!(rendered_text(&app.status_line(80)).contains("stopped"));
        app.set_activity(Activity::Paused);
        app.set_notice("paused", NoticeLevel::Warning);
        assert!(rendered_text(&app.status_line(80)).contains("paused"));
    }

    #[test]
    fn narrow_terminal_keeps_transcript_status_and_input_visible() {
        let backend = ratatui::backend::TestBackend::new(40, 9);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.configure_runtime(
            "openai/gpt-5.6-sol".into(),
            None,
            std::path::PathBuf::from("/workspace/very-long-project-name"),
            204_000,
            Some(272_000),
            false,
        );
        app.busy = true;
        app.push_loop_event(&LoopEvent::TurnStarted);
        terminal.draw(|frame| app.render(frame)).unwrap();

        let buffer = terminal.backend().buffer();
        let screen = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains("ilar"), "{screen}");
        assert!(screen.contains("thinking"), "{screen}");
        assert!(screen.contains("75%"), "{screen}");
        assert!(screen.contains("input"), "{screen}");
    }

    #[test]
    fn scrolling_detaches_and_resumes_tail_follow() {
        let mut app = App::new();
        app.content_rows = 100;
        app.viewport_rows = 20;
        app.scroll_top = 80;
        app.follow_tail = true;

        app.scroll_up(18);
        assert_eq!(app.scroll_top, 62);
        assert!(!app.follow_tail);

        app.scroll_down(18);
        assert_eq!(app.scroll_top, 80);
        assert!(app.follow_tail);
    }

    #[test]
    fn scrolling_clamps_at_top_and_bottom() {
        let mut app = App::new();
        app.content_rows = 30;
        app.viewport_rows = 10;
        app.scroll_top = 20;

        app.scroll_up(100);
        assert_eq!(app.scroll_top, 0);
        app.scroll_down(100);
        assert_eq!(app.scroll_top, 20);
        assert!(app.follow_tail);
    }

    #[test]
    fn queued_wheel_events_are_coalesced_until_the_next_distinct_input() {
        let key = Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        let mut queued = vec![
            Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            }),
            Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            }),
            key,
            Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            }),
        ]
        .into_iter();

        let batch = drain_wheel_batch(-3, 32, || Ok(queued.next())).unwrap();

        assert_eq!(batch.rows, -3);
        assert!(matches!(
            batch.deferred,
            Some(Event::Key(KeyEvent {
                code: KeyCode::Char('x'),
                ..
            }))
        ));
        assert!(matches!(
            queued.next(),
            Some(Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::ScrollUp,
                ..
            }))
        ));
    }

    #[test]
    fn wheel_batch_work_is_bounded_and_zero_net_clears_selection() {
        let mut reads = 0;
        let batch = drain_wheel_batch(-3, 4, || {
            reads += 1;
            Ok(Some(Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            })))
        })
        .unwrap();
        assert_eq!(reads, 3);
        assert_eq!(batch.rows, -12);
        assert!(batch.deferred.is_none());

        let mut app = App::new();
        app.scroll_top = 7;
        app.follow_tail = false;
        app.transcript_selection = Some(TranscriptSelection {
            anchor: SelectionPoint { row: 0, column: 0 },
            focus: SelectionPoint { row: 0, column: 1 },
        });

        app.scroll_wheel(0);

        assert_eq!(app.scroll_top, 7);
        assert!(!app.follow_tail);
        assert_eq!(app.transcript_selection, None);
    }

    fn cells(rows: &[&str], width: usize) -> Vec<RenderedRow> {
        rows.iter()
            .map(|row| {
                row.chars()
                    .map(|character| match character {
                        ' ' => RenderedCell::Space,
                        _ => RenderedCell::Character(character),
                    })
                    .chain(std::iter::repeat(RenderedCell::Space))
                    .take(width)
                    .collect()
            })
            .collect()
    }

    #[test]
    fn transcript_selection_copies_multiline_text_in_display_order() {
        let rows = cells(&["abc", "wxyz"], 6);
        let forward = TranscriptSelection {
            anchor: SelectionPoint { row: 0, column: 1 },
            focus: SelectionPoint { row: 1, column: 2 },
        };
        let reverse = TranscriptSelection {
            anchor: forward.focus,
            focus: forward.anchor,
        };

        assert_eq!(
            selected_transcript_text(&rows, forward).as_deref(),
            Some("bc\nwxy")
        );
        assert_eq!(
            selected_transcript_text(&rows, reverse).as_deref(),
            Some("bc\nwxy")
        );
    }

    #[test]
    fn transcript_selection_ignores_clicks_and_trailing_viewport_padding() {
        let rows = cells(&["ilar hello"], 16);
        let click = TranscriptSelection {
            anchor: SelectionPoint { row: 0, column: 3 },
            focus: SelectionPoint { row: 0, column: 3 },
        };
        let drag = TranscriptSelection {
            anchor: SelectionPoint { row: 0, column: 5 },
            focus: SelectionPoint { row: 0, column: 15 },
        };

        assert_eq!(selected_transcript_text(&rows, click), None);
        assert_eq!(
            selected_transcript_text(&rows, drag).as_deref(),
            Some("hello")
        );
    }

    #[test]
    fn transcript_mouse_points_are_clamped_to_the_visible_text_area() {
        let area = Rect::new(10, 4, 8, 3);
        assert_eq!(
            selection_point(area, 12, 5, false),
            Some(SelectionPoint { row: 1, column: 2 })
        );
        assert_eq!(selection_point(area, 9, 5, false), None);
        assert_eq!(
            selection_point(area, 30, 20, true),
            Some(SelectionPoint { row: 2, column: 7 })
        );
    }

    #[test]
    fn transcript_selection_highlights_cells_and_scrolling_clears_it() {
        let area = Rect::new(0, 0, 4, 2);
        let mut buffer = Buffer::empty(area);
        buffer.set_string(0, 0, "abcd", Style::default());
        buffer.set_string(0, 1, "efgh", Style::default());
        let selection = TranscriptSelection {
            anchor: SelectionPoint { row: 0, column: 1 },
            focus: SelectionPoint { row: 1, column: 1 },
        };
        let rows = transcript_cells(&buffer, area);
        highlight_transcript_selection(&mut buffer, area, selection, &rows);

        assert!(!buffer[(0, 0)].modifier.contains(Modifier::REVERSED));
        for position in [(1, 0), (2, 0), (3, 0), (0, 1), (1, 1)] {
            assert!(
                buffer[position].modifier.contains(Modifier::REVERSED),
                "missing highlight at {position:?}"
            );
        }
        assert!(!buffer[(2, 1)].modifier.contains(Modifier::REVERSED));

        let mut app = App::new();
        app.transcript_selection = Some(selection);
        app.selecting_transcript = true;
        app.scroll_down(3);
        assert_eq!(app.transcript_selection, None);
        assert!(!app.selecting_transcript);
    }

    #[test]
    fn transcript_selection_preserves_wide_graphemes_without_phantom_spaces() {
        let area = Rect::new(0, 0, 4, 1);
        let mut buffer = Buffer::empty(area);
        buffer.set_string(0, 0, "界B", Style::default());
        let rows = transcript_cells(&buffer, area);
        let selection = TranscriptSelection {
            anchor: SelectionPoint { row: 0, column: 1 },
            focus: SelectionPoint { row: 0, column: 2 },
        };

        assert_eq!(
            selected_transcript_text(&rows, selection).as_deref(),
            Some("界B")
        );
        highlight_transcript_selection(&mut buffer, area, selection, &rows);
        for column in 0..=2 {
            assert!(buffer[(column, 0)].modifier.contains(Modifier::REVERSED));
        }
    }

    #[test]
    fn transcript_selection_does_not_copy_vertical_viewport_padding() {
        let rows = cells(&["hello"], 8);
        let selection = TranscriptSelection {
            anchor: SelectionPoint { row: 0, column: 0 },
            focus: SelectionPoint { row: 4, column: 7 },
        };
        assert_eq!(
            selected_transcript_text(&rows, selection).as_deref(),
            Some("hello")
        );
    }

    #[test]
    fn transcript_selection_is_cancelled_when_visible_output_changes() {
        let backend = ratatui::backend::TestBackend::new(40, 9);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut app = App::new();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let area = app.transcript_text_area;
        app.begin_transcript_selection(area.x, area.y);
        app.update_transcript_selection(area.x.saturating_add(3), area.y);
        assert!(app.transcript_selection.is_some());
        let previous = app.transcript_cells.clone();

        app.lines[0] = Line_::System("changed output".into());
        app.transcript_revision = app.transcript_revision.wrapping_add(1);
        terminal.draw(|frame| app.render(frame)).unwrap();

        assert_ne!(previous, app.transcript_cells);
        assert_eq!(app.transcript_selection, None);
        assert!(!app.selecting_transcript);
    }

    #[test]
    fn transcript_selection_ignores_changes_outside_selected_rows() {
        let previous = cells(&["stable", "thinking one"], 16);
        let current = cells(&["stable", "thinking two"], 16);
        let stable_selection = TranscriptSelection {
            anchor: SelectionPoint { row: 0, column: 0 },
            focus: SelectionPoint { row: 0, column: 3 },
        };
        let volatile_selection = TranscriptSelection {
            anchor: SelectionPoint { row: 1, column: 0 },
            focus: SelectionPoint { row: 1, column: 3 },
        };

        assert!(selected_rows_unchanged(
            &previous,
            &current,
            stable_selection
        ));
        assert!(!selected_rows_unchanged(
            &previous,
            &current,
            volatile_selection
        ));
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
    fn transcript_lines_exclude_current_todos() {
        let app = App::new();
        app.todos.lock().unwrap().items = vec![ilar::todo::TodoItem {
            content: "must stay fixed".into(),
            status: ilar::todo::Status::InProgress,
        }];

        let rendered = app
            .transcript_lines(80, std::time::Instant::now())
            .iter()
            .map(rendered_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(!rendered.contains("must stay fixed"), "{rendered}");
    }

    #[test]
    fn transcript_cache_rebuilds_only_the_changed_streaming_entry() {
        let mut app = App::new();
        app.lines = vec![
            Line_::Assistant("final **answer**".into()),
            Line_::Tool {
                id: "done".into(),
                group_id: "test:0".into(),
                name: "read".into(),
                kind: ToolKind::Tool,
                arguments: "src/main.rs".into(),
                argument_detail: String::new(),
                diff: Vec::new(),
                tail: String::new(),
                result: None,
                state: ToolState::Succeeded,
                progress: ToolProgress::None,
                expanded: false,
                full: false,
                child_lines: Vec::new(),
                child_group: 0,
                child_running: false,
                child_session_id: None,
            },
            Line_::Assistant("stream".into()),
        ];
        let now = std::time::Instant::now();
        app.transcript_cache.update(
            &app.lines,
            &app.expanded_tool_groups,
            app.transcript_revision,
            40,
            now,
            app.activity_started,
        );
        assert_eq!(app.transcript_cache.rebuilds, 3);

        app.push_loop_event(&LoopEvent::TextDelta("ing".into()));
        app.transcript_cache.update(
            &app.lines,
            &app.expanded_tool_groups,
            app.transcript_revision,
            40,
            now,
            app.activity_started,
        );

        assert_eq!(app.transcript_cache.rebuilds, 4);
    }

    #[test]
    fn idle_transcript_cache_returns_only_viewport_rows() {
        let mut app = App::new();
        app.lines = (0..1_000)
            .map(|index| Line_::System(format!("row {index}")))
            .collect();
        let now = std::time::Instant::now();
        app.transcript_cache.update(
            &app.lines,
            &app.expanded_tool_groups,
            app.transcript_revision,
            40,
            now,
            app.activity_started,
        );
        let rebuilds = app.transcript_cache.rebuilds;

        let visible = app.transcript_cache.visible_rows(500, 7, &[]);
        app.transcript_cache.update(
            &app.lines,
            &app.expanded_tool_groups,
            app.transcript_revision,
            40,
            now,
            app.activity_started,
        );

        assert_eq!(visible.len(), 7);
        assert_eq!(app.transcript_cache.rebuilds, rebuilds);
    }

    #[test]
    fn cached_transcript_output_matches_the_existing_renderer() {
        let mut app = App::new();
        app.lines = vec![
            Line_::User("hello\nthere".into()),
            Line_::Assistant("## Result\n\nA **styled** response that wraps.".into()),
            Line_::System("finished".into()),
        ];
        let width = 24;
        let now = std::time::Instant::now();
        let expected = app
            .transcript_lines(width, now)
            .into_iter()
            .flat_map(|line| wrap_styled_line(line, width as usize))
            .collect::<Vec<_>>();

        app.transcript_cache.update(
            &app.lines,
            &app.expanded_tool_groups,
            app.transcript_revision,
            width,
            now,
            app.activity_started,
        );
        let actual = app
            .transcript_cache
            .visible_rows(0, usize::MAX, &[])
            .into_iter()
            .map(|row| row.line)
            .collect::<Vec<_>>();

        assert_eq!(actual, expected);
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

    #[test]
    fn wide_transcript_keeps_two_cell_horizontal_margins() {
        let backend = ratatui::backend::TestBackend::new(140, 12);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.lines.push(Line_::Assistant("visible prose".into()));

        terminal.draw(|frame| app.render(frame)).unwrap();

        assert_eq!(app.transcript_text_area.x, 3);
        assert_eq!(app.transcript_text_area.width, 92);
    }

    #[test]
    fn todo_updates_do_not_change_transcript_scroll_or_selection() {
        let backend = ratatui::backend::TestBackend::new(100, 12);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.lines
            .extend((0..20).map(|index| Line_::System(format!("transcript row {index}"))));
        terminal.draw(|frame| app.render(frame)).unwrap();
        app.scroll_up(2);
        terminal.draw(|frame| app.render(frame)).unwrap();
        let area = app.transcript_text_area;
        app.begin_transcript_selection(area.x, area.y);
        app.update_transcript_selection(area.x.saturating_add(3), area.y);
        let metrics = (
            app.content_rows,
            app.viewport_rows,
            app.scroll_top,
            app.follow_tail,
            app.transcript_selection,
        );

        app.todos.lock().unwrap().items = vec![ilar::todo::TodoItem {
            content: "fixed sidebar item".into(),
            status: ilar::todo::Status::InProgress,
        }];
        terminal.draw(|frame| app.render(frame)).unwrap();

        assert_eq!(
            (
                app.content_rows,
                app.viewport_rows,
                app.scroll_top,
                app.follow_tail,
                app.transcript_selection,
            ),
            metrics
        );
    }

    #[test]
    fn narrow_todos_use_border_chrome_instead_of_transcript_rows() {
        let backend = ratatui::backend::TestBackend::new(40, 9);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.todos.lock().unwrap().items = vec![ilar::todo::TodoItem {
            content: "border task".into(),
            status: ilar::todo::Status::InProgress,
        }];
        terminal.draw(|frame| app.render(frame)).unwrap();

        let buffer = terminal.backend().buffer();
        let screen = (0..buffer.area.height)
            .map(|row| {
                (0..buffer.area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains("todos ▸ border task"), "{screen}");
        let transcript = app
            .transcript_lines(40, std::time::Instant::now())
            .iter()
            .map(rendered_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!transcript.contains("border task"), "{transcript}");
    }

    #[test]
    fn one_row_transcript_omits_overlapping_todo_title() {
        let backend = ratatui::backend::TestBackend::new(40, 5);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.todos.lock().unwrap().items = vec![ilar::todo::TodoItem {
            content: "border task".into(),
            status: ilar::todo::Status::InProgress,
        }];
        terminal.draw(|frame| app.render(frame)).unwrap();

        let buffer = terminal.backend().buffer();
        let first_row = (0..buffer.area.width)
            .map(|column| buffer[(column, 0)].symbol())
            .collect::<String>();
        assert!(first_row.contains("ilar"), "{first_row}");
        assert!(!first_row.contains("border task"), "{first_row}");
    }

    #[test]
    fn wide_todos_render_in_sidebar_and_sidebar_clicks_do_not_select_transcript() {
        let backend = ratatui::backend::TestBackend::new(140, 12);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.todos.lock().unwrap().items = vec![ilar::todo::TodoItem {
            content: "sidebar task".into(),
            status: ilar::todo::Status::InProgress,
        }];
        terminal.draw(|frame| app.render(frame)).unwrap();

        let buffer = terminal.backend().buffer();
        let screen = (0..buffer.area.height)
            .map(|row| {
                (0..buffer.area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains("todos"), "{screen}");
        assert!(screen.contains("▸ sidebar task"), "{screen}");
        assert_eq!(app.transcript_text_area.width, 92);

        app.begin_transcript_selection(110, 1);
        assert_eq!(app.transcript_selection, None);
    }

    #[test]
    fn resumed_todos_seed_the_first_shared_list() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let session_id = new_id();
        let mut session = store
            .create(SessionMeta {
                session_id: session_id.clone(),
                parent_id: None,
                agent: "build".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::AssistantMessage {
                id: new_id(),
                model: "zai/glm-4.7".into(),
                content: vec![ilar::session::ContentBlock::ToolCall {
                    id: "todo-resume".into(),
                    name: "todo".into(),
                    input: Default::default(),
                }],
                usage: ilar::session::Usage::default(),
                stop_reason: "tool_use".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::ToolResult {
                id: new_id(),
                tool_use_id: "todo-resume".into(),
                content: "updated".into(),
                is_error: false,
                child_session_id: None,
                state: Some(ilar::session::SessionState::TodoList {
                    list: ilar::todo::TodoList {
                        items: vec![ilar::todo::TodoItem {
                            content: "restored".into(),
                            status: ilar::todo::Status::InProgress,
                        }],
                    },
                }),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(session);
        let resumed = store.load(&session_id).unwrap();

        let restored = restored_todos(Some(&resumed));
        assert_eq!(restored.items.len(), 1);
        assert_eq!(restored.items[0].content, "restored");
        assert_eq!(restored.items[0].status, ilar::todo::Status::InProgress);
    }

    #[test]
    fn resize_clamps_without_reattaching_detached_viewport() {
        let mut app = App::new();
        app.content_rows = 100;
        app.viewport_rows = 20;
        app.scroll_top = 70;
        app.follow_tail = false;

        app.update_scroll_metrics(30, 20);

        assert_eq!(app.scroll_top, 10);
        assert!(!app.follow_tail);
    }

    #[test]
    fn notification_burst_stays_queued_while_a_turn_is_active() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);
        for description in ["first", "second"] {
            tx.try_send(ilar::subagent::Notification {
                parent_session_id: "parent".into(),
                description: description.into(),
                text: description.into(),
                is_error: false,
            })
            .unwrap();
        }

        let mut pending = None;
        assert!(next_notification(true, false, &mut pending, &mut rx).is_none());
        assert_eq!(rx.len(), 2);
        assert!(next_notification(false, true, &mut pending, &mut rx).is_none());
        assert_eq!(rx.len(), 2);
        assert_eq!(
            next_notification(false, false, &mut pending, &mut rx)
                .unwrap()
                .description,
            "first"
        );
        assert_eq!(
            next_notification(false, false, &mut pending, &mut rx)
                .unwrap()
                .description,
            "second"
        );
    }

    #[test]
    fn propagated_notification_follows_the_existing_receiver_backlog() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);
        tx.try_send(ilar::subagent::Notification {
            parent_session_id: "parent".into(),
            description: "queued".into(),
            text: "queued".into(),
            is_error: false,
        })
        .unwrap();
        let mut pending = Some(PendingNotification {
            notification: ilar::subagent::Notification {
                parent_session_id: "parent".into(),
                description: "propagated".into(),
                text: "propagated".into(),
                is_error: false,
            },
            queued_ahead: rx.len(),
        });

        assert_eq!(
            next_notification(false, false, &mut pending, &mut rx)
                .unwrap()
                .description,
            "queued"
        );
        assert_eq!(
            next_notification(false, false, &mut pending, &mut rx)
                .unwrap()
                .description,
            "propagated"
        );
    }
}
