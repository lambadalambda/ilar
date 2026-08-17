//! ilar TUI: transcript, streaming, tool display, input. Esc aborts.

mod markdown;

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseButton,
    MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::supports_keyboard_enhancement;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, Padding, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use ratatui::{Frame, buffer::Buffer};
use tokio_util::sync::CancellationToken;
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

/// A rendered line in the transcript.
#[derive(Debug, Clone, PartialEq)]
enum Line_ {
    User(String),
    Assistant(String),
    Tool {
        id: String,
        name: String,
        arguments: String,
        state: ToolState,
    },
    System(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolState {
    Running,
    Succeeded,
    Failed,
}

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

const MUTED: Color = Color::DarkGray;
const ASSISTANT: Color = Color::Green;
const TOOL_ACTIVE: Color = Color::Yellow;
const ERROR: Color = Color::Red;
const TODO_SIDEBAR_MIN_WIDTH: u16 = 80;
const TODO_SIDEBAR_WIDTH: u16 = 28;
const TODO_SIDEBAR_MAX_ITEMS: usize = 5;

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
    #[cfg(test)]
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

struct RestoredSessionView {
    lines: Vec<Line_>,
    latest_usage: Option<ilar::session::Usage>,
}

fn restored_session_view(session: &ilar::session::SessionReader) -> RestoredSessionView {
    let events = session.events();
    let mut cut = 0usize;
    let mut summary = None;
    for (index, event) in events.iter().enumerate() {
        if let ilar::session::SessionEvent::Compaction {
            kept_from,
            summary: current,
            ..
        } = event
        {
            cut = (*kept_from).min(index).max(cut);
            summary = Some(current.as_str());
        }
    }
    let latest_usage = events.iter().rev().find_map(|event| match event {
        ilar::session::SessionEvent::AssistantMessage { usage, .. }
            if usage.context_tokens() > 0 =>
        {
            Some(*usage)
        }
        _ => None,
    });
    let mut lines = summary
        .map(|summary| vec![Line_::System(format!("transcript compacted\n{summary}"))])
        .unwrap_or_default();
    for event in &events[cut..] {
        match event {
            ilar::session::SessionEvent::Meta { .. } => {}
            ilar::session::SessionEvent::UserMessage { text, .. } => {
                lines.push(Line_::User(text.clone()));
            }
            ilar::session::SessionEvent::AssistantMessage { content, .. } => {
                for block in content {
                    match block {
                        ilar::session::ContentBlock::Text { text } => match lines.last_mut() {
                            Some(Line_::Assistant(current)) => current.push_str(text),
                            _ => lines.push(Line_::Assistant(text.clone())),
                        },
                        ilar::session::ContentBlock::ToolCall { id, name, input } => {
                            lines.push(Line_::Tool {
                                id: id.clone(),
                                name: name.clone(),
                                arguments: ilar::agent::summarize_tool_input(name, input),
                                state: ToolState::Running,
                            });
                        }
                        ilar::session::ContentBlock::Thinking { .. }
                        | ilar::session::ContentBlock::Reasoning { .. }
                        | ilar::session::ContentBlock::ProviderReplay { .. }
                        | ilar::session::ContentBlock::Diagnostic { .. }
                        | ilar::session::ContentBlock::ToolResult { .. } => {}
                    }
                }
            }
            ilar::session::SessionEvent::ToolResult {
                tool_use_id,
                is_error,
                ..
            } => {
                if let Some(state) = lines.iter_mut().rev().find_map(|line| match line {
                    Line_::Tool { id, state, .. } if id == tool_use_id => Some(state),
                    _ => None,
                }) {
                    *state = if *is_error {
                        ToolState::Failed
                    } else {
                        ToolState::Succeeded
                    };
                }
            }
            ilar::session::SessionEvent::ModelChange { model, .. } => {
                lines.push(Line_::System(format!("switched to {model}")));
            }
            ilar::session::SessionEvent::Compaction { .. } => {}
        }
    }
    for line in &mut lines {
        if let Line_::Tool { state, .. } = line
            && *state == ToolState::Running
        {
            *state = ToolState::Failed;
        }
    }
    RestoredSessionView {
        lines,
        latest_usage,
    }
}

struct App {
    lines: Vec<Line_>,
    input: InputBuffer,
    busy: bool,
    status: String,
    activity: Activity,
    activity_started: std::time::Instant,
    current_model: String,
    cwd: std::path::PathBuf,
    context_used: u64,
    context_limit: Option<u64>,
    context_estimated: bool,
    latest_usage: Option<ilar::session::Usage>,
    scroll_top: usize,
    content_rows: usize,
    viewport_rows: usize,
    follow_tail: bool,
    model_picker: Option<ModelPicker>,
    model_key_pending: bool,
    transcript_text_area: Rect,
    transcript_cells: Vec<RenderedRow>,
    transcript_selection: Option<TranscriptSelection>,
    selecting_transcript: bool,
    clipboard: Option<arboard::Clipboard>,
    todos: std::sync::Arc<std::sync::Mutex<ilar::todo::TodoList>>,
}

impl App {
    fn new() -> Self {
        Self {
            lines: vec![Line_::System(
                "ilar — Enter sends, Shift-Enter/Ctrl-J newline, F2 models, PgUp/PgDn scroll"
                    .into(),
            )],
            input: InputBuffer::default(),
            busy: false,
            status: String::new(),
            activity: Activity::Ready,
            activity_started: std::time::Instant::now(),
            current_model: "unknown".into(),
            cwd: std::path::PathBuf::from("."),
            context_used: 0,
            context_limit: None,
            context_estimated: true,
            latest_usage: None,
            scroll_top: 0,
            content_rows: 0,
            viewport_rows: 0,
            follow_tail: true,
            model_picker: None,
            model_key_pending: false,
            transcript_text_area: Rect::default(),
            transcript_cells: Vec::new(),
            transcript_selection: None,
            selecting_transcript: false,
            clipboard: None,
            todos: std::sync::Arc::new(std::sync::Mutex::new(ilar::todo::TodoList::default())),
        }
    }

    fn configure_runtime(
        &mut self,
        model: String,
        cwd: std::path::PathBuf,
        context_used: u64,
        context_limit: Option<u64>,
        context_estimated: bool,
    ) {
        self.current_model = model;
        self.cwd = cwd;
        self.context_used = context_used;
        self.context_limit = context_limit;
        self.context_estimated = context_estimated;
        self.status = "ready".into();
    }

    fn restore_session(&mut self, session: &ilar::session::SessionReader) {
        let restored = restored_session_view(session);
        self.lines.extend(restored.lines);
        self.latest_usage = restored.latest_usage;
    }

    fn set_activity(&mut self, activity: Activity) {
        if self.activity != activity {
            self.activity = activity;
            self.activity_started = std::time::Instant::now();
        }
    }

    fn push_loop_event(&mut self, event: &LoopEvent) {
        match event {
            LoopEvent::TurnStarted => {
                self.status = "thinking…".into();
                self.set_activity(Activity::Thinking);
            }
            LoopEvent::TextDelta(t) => {
                self.status = "responding".into();
                self.set_activity(Activity::Responding);
                match self.lines.last_mut() {
                    Some(Line_::Assistant(text)) => text.push_str(t),
                    _ => self.lines.push(Line_::Assistant(t.clone())),
                }
            }
            LoopEvent::ThinkingDelta(_) => {
                self.status = "thinking".into();
                self.set_activity(Activity::Thinking);
            }
            LoopEvent::ToolStarted { id, name } => {
                self.lines.push(Line_::Tool {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: String::new(),
                    state: ToolState::Running,
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
            LoopEvent::ToolFinished { id, name, is_error } => {
                let mut matched = false;
                if let Some(state) = self.lines.iter_mut().rev().find_map(|line| match line {
                    Line_::Tool {
                        id: line_id, state, ..
                    } if line_id == id && *state == ToolState::Running => Some(state),
                    _ => None,
                }) {
                    *state = if *is_error {
                        ToolState::Failed
                    } else {
                        ToolState::Succeeded
                    };
                    matched = true;
                }
                if !matched {
                    self.lines.push(Line_::Tool {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: String::new(),
                        state: if *is_error {
                            ToolState::Failed
                        } else {
                            ToolState::Succeeded
                        },
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
                self.latest_usage = Some(*usage);
                let reported = usage.context_tokens();
                if reported > 0 {
                    self.context_used = reported;
                    self.context_estimated = false;
                } else {
                    self.context_estimated = true;
                }
                self.status = format!(
                    "{stop_reason} · in {} out {} (cache {})",
                    usage.input_tokens, usage.output_tokens, usage.cache_read_input_tokens
                );
            }
            LoopEvent::Compacted { context_tokens } => {
                self.context_used = *context_tokens;
                self.context_estimated = true;
                self.lines
                    .push(Line_::System("transcript compacted".into()));
            }
            LoopEvent::TurnDone { outcome } => {
                if *outcome == TurnOutcome::Aborted {
                    for line in &mut self.lines {
                        if let Line_::Tool { state, .. } = line
                            && *state == ToolState::Running
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
                self.set_activity(match outcome {
                    TurnOutcome::Completed => Activity::Ready,
                    TurnOutcome::Aborted => Activity::Aborted,
                    TurnOutcome::MaxIterations => Activity::Stopped,
                });
            }
        }
    }

    fn finish_turn(&mut self, result: anyhow::Result<TurnOutcome>) {
        if let Err(error) = result {
            for line in &mut self.lines {
                if let Line_::Tool { state, .. } = line
                    && *state == ToolState::Running
                {
                    *state = ToolState::Failed;
                }
            }
            self.lines.push(Line_::System(format!("error: {error:#}")));
            self.status = "error".into();
            self.set_activity(Activity::Error);
        }
        self.busy = false;
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

    fn finish_transcript_selection(&mut self, column: u16, row: u16) -> Option<String> {
        if !self.selecting_transcript {
            return None;
        }
        self.update_transcript_selection(column, row);
        self.selecting_transcript = false;
        let selection = self.transcript_selection?;
        let text = selected_transcript_text(&self.transcript_cells, selection);
        if text.is_none() {
            self.transcript_selection = None;
        }
        text
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

    fn transcript_lines(&self, width: u16, now: std::time::Instant) -> Vec<Line<'static>> {
        let mut output = Vec::new();
        for entry in &self.lines {
            match entry {
                Line_::Assistant(text) => {
                    let mut first = true;
                    let label_width = 5usize.min(width.saturating_sub(2) as usize);
                    for line in markdown::render(text) {
                        if line.spans.is_empty() {
                            output.push(Line::default());
                            continue;
                        }
                        for mut line in
                            wrap_styled_line(line, (width as usize).saturating_sub(label_width))
                        {
                            let label = if first {
                                truncate_display("ilar ", label_width, Truncation::Right)
                            } else {
                                " ".repeat(label_width)
                            };
                            first = false;
                            let mut spans = vec![Span::styled(
                                label,
                                Style::default()
                                    .fg(Color::Green)
                                    .add_modifier(Modifier::BOLD),
                            )];
                            spans.append(&mut line.spans);
                            output.push(Line::from(spans));
                        }
                    }
                }
                Line_::User(text) => {
                    for (index, text) in safe_lines(text).into_iter().enumerate() {
                        output.push(Line::from(vec![
                            Span::styled(
                                if index == 0 { "you  " } else { "     " },
                                Style::default()
                                    .fg(Color::Cyan)
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::raw(text),
                        ]));
                    }
                }
                Line_::Tool {
                    name,
                    arguments,
                    state,
                    ..
                } => output.push(tool_line(
                    name,
                    arguments,
                    *state,
                    width,
                    now.saturating_duration_since(self.activity_started),
                )),
                Line_::System(text) => {
                    for (index, text) in safe_lines(text).into_iter().enumerate() {
                        output.push(Line::from(vec![
                            Span::styled(
                                if index == 0 { "—    " } else { "     " },
                                Style::default().fg(Color::DarkGray),
                            ),
                            Span::styled(text, Style::default().fg(Color::DarkGray)),
                        ]));
                    }
                }
            }
        }
        if self.busy
            && matches!(
                self.activity,
                Activity::Thinking | Activity::Responding | Activity::Tools
            )
        {
            let elapsed = now.saturating_duration_since(self.activity_started);
            let (frame, label, color) = match self.activity {
                Activity::Thinking => {
                    let frames = ["◐", "◓", "◑", "◒"];
                    (
                        frames[(elapsed.as_millis() / 160) as usize % frames.len()],
                        "thinking…",
                        TOOL_ACTIVE,
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
                        "running tools…",
                        TOOL_ACTIVE,
                    )
                }
                _ => unreachable!(),
            };
            output.push(Line::from(vec![
                Span::styled(
                    "ilar ",
                    Style::default().fg(ASSISTANT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("{frame} "), Style::default().fg(color)),
                Span::styled(label, Style::default().fg(MUTED)),
            ]));
        }
        output
    }

    fn status_line(&self, width: u16) -> Line<'static> {
        let width = width as usize;
        let (icon, state, state_color) = match self.activity {
            Activity::Ready => ("●", "ready", ASSISTANT),
            Activity::Thinking => ("○", "thinking", TOOL_ACTIVE),
            Activity::Responding => ("▸", "responding", ASSISTANT),
            Activity::Tools => ("◆", "tools", TOOL_ACTIVE),
            Activity::Aborting => ("■", "aborting", TOOL_ACTIVE),
            Activity::Aborted => ("■", "aborted", TOOL_ACTIVE),
            Activity::Stopped => ("■", "stopped", TOOL_ACTIVE),
            Activity::Paused => ("Ⅱ", "paused", TOOL_ACTIVE),
            Activity::Error => ("×", "error", ERROR),
        };
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
            Some(percent) if percent >= 70 => TOOL_ACTIVE,
            _ => MUTED,
        };
        let percent = self
            .context_limit
            .filter(|limit| *limit > 0)
            .map(|limit| format!("{}%", self.context_used.saturating_mul(100) / limit))
            .unwrap_or_else(|| "—%".into());
        let compact_latest_usage = self.latest_usage.map(|latest| {
            format!(
                "i{}/o{} c{} {percent}",
                format_tokens_compact(latest.input_tokens),
                format_tokens_compact(latest.output_tokens),
                format_tokens_compact(
                    latest
                        .cache_read_input_tokens
                        .saturating_add(latest.cache_creation_input_tokens)
                )
            )
        });
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
            let short_model = self
                .current_model
                .split_once('/')
                .map(|(_, model)| model)
                .unwrap_or(&self.current_model);
            let model_budget = if self.latest_usage.is_some() {
                available
            } else {
                available.saturating_mul(3) / 5
            };
            let model = truncate_display(short_model, model_budget.max(1), Truncation::Right);
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
            let middle = cwd
                .as_deref()
                .map(|cwd| format!(" {model} {cwd}"))
                .unwrap_or_else(|| format!(" {model}"));
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
        let detailed_usage = self.latest_usage.map(|latest| {
            format!(
                "in {} · out {} · cache {} · {context}",
                latest.input_tokens,
                latest.output_tokens,
                latest
                    .cache_read_input_tokens
                    .saturating_add(latest.cache_creation_input_tokens)
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
        let usage = detailed_usage.or(compact_latest_usage).unwrap_or(context);
        let usage = truncate_display(
            &usage,
            width.saturating_sub(state_width + separators + 8),
            Truncation::Right,
        );
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let cwd = show_cwd.then(|| abbreviated_path(&self.cwd, home.as_deref()));
        let mut model = self.current_model.clone();

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
        if width < 80 {
            model = model
                .split_once('/')
                .map(|(_, model)| model.to_string())
                .unwrap_or(model);
        }
        let model = truncate_display(&model, model_budget.max(4), Truncation::Right);
        let cwd = cwd.map(|cwd| {
            truncate_display(
                &cwd,
                available
                    .saturating_sub(UnicodeWidthStr::width(model.as_str()))
                    .max(4),
                Truncation::Middle,
            )
        });
        let detail = cwd
            .as_deref()
            .map(|cwd| format!(" · {model} · {cwd}"))
            .unwrap_or_else(|| format!(" · {model}"));
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
        let text_width = transcript_area.width.saturating_sub(3);
        let text = self
            .transcript_lines(text_width, std::time::Instant::now())
            .into_iter()
            .flat_map(|line| wrap_styled_line(line, text_width as usize))
            .collect::<Vec<_>>();
        let viewport_rows = transcript_area.height.saturating_sub(2) as usize;
        let content_rows = text.len();
        self.update_scroll_metrics(content_rows, viewport_rows);
        let visible_rows = content_rows
            .saturating_sub(self.scroll_top)
            .min(viewport_rows) as u16;
        let transcript_text_area = Rect::new(
            transcript_area.x.saturating_add(1),
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
            .title(format!("ilar{scroll_label}"))
            .padding(Padding::new(0, 1, 0, 0));
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
        let text = text
            .into_iter()
            .skip(self.scroll_top)
            .take(viewport_rows)
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
            let todo_block =
                Block::default()
                    .borders(Borders::ALL)
                    .title(Line::from(Span::styled(
                        "todos",
                        Style::default()
                            .fg(TOOL_ACTIVE)
                            .add_modifier(Modifier::BOLD),
                    )));
            let inner = todo_block.inner(todo_area);
            let snapshot = {
                let todos = self.todos.lock().unwrap();
                todo_render_snapshot(&todos, TODO_SIDEBAR_MAX_ITEMS.min(inner.height as usize))
            };
            let lines = render_todo_sidebar_snapshot(&snapshot, inner.width, inner.height);
            frame.render_widget(Paragraph::new(lines).block(todo_block), todo_area);
        }

        frame.render_widget(Paragraph::new(self.status_line(chunks[1].width)), chunks[1]);

        let input_block = Block::default().borders(Borders::ALL);
        let input_area = input_block.inner(chunks[2]);
        let input_view = self
            .input
            .multiline_view(input_area.width, input_area.height);
        let input_title = if input_view.line_count > 1 {
            format!(
                "input {}/{} · Enter send · Shift-Enter/Ctrl-J newline",
                input_view.cursor_line, input_view.line_count
            )
        } else {
            "input · Enter send · Shift-Enter/Ctrl-J newline".into()
        };
        let input_lines = input_view
            .lines
            .iter()
            .cloned()
            .map(Line::raw)
            .collect::<Vec<_>>();
        let input = Paragraph::new(input_lines).block(input_block.title(input_title));
        frame.render_widget(input, chunks[2]);

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

        if !self.busy
            && self.model_picker.is_none()
            && input_area.width > 0
            && input_area.height > 0
        {
            frame.set_cursor_position((
                input_area.x.saturating_add(input_view.cursor_x),
                input_area.y.saturating_add(input_view.cursor_y),
            ));
        }

        if let Some(picker) = &self.model_picker {
            render_model_picker(frame, picker);
        }
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

#[cfg(test)]
fn render_todo_sidebar_lines(
    list: &ilar::todo::TodoList,
    width: u16,
    height: u16,
) -> Vec<Line<'static>> {
    let snapshot = todo_render_snapshot(list, TODO_SIDEBAR_MAX_ITEMS.min(height as usize));
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
    let last = snapshot.items.len().saturating_sub(1);
    snapshot
        .items
        .iter()
        .enumerate()
        .map(|(position, item)| {
            let (marker, marker_style, content_style) = match item.status {
                ilar::todo::Status::Completed => (
                    "✓ ",
                    Style::default().fg(ASSISTANT),
                    Style::default().fg(MUTED),
                ),
                ilar::todo::Status::InProgress => (
                    "▸ ",
                    Style::default().fg(TOOL_ACTIVE),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                ilar::todo::Status::Pending => ("○ ", Style::default().fg(MUTED), Style::default()),
            };
            let remaining = width as usize;
            let marker = truncate_display(marker, remaining, Truncation::Right);
            let remaining = remaining.saturating_sub(UnicodeWidthStr::width(marker.as_str()));
            let suffix = if position == last && snapshot.hidden > 0 {
                format!(" · +{} hidden", snapshot.hidden)
            } else {
                String::new()
            };
            let suffix_width = UnicodeWidthStr::width(suffix.as_str()).min(remaining);
            let content = item
                .content
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let content = safe_text(&content);
            let content = truncate_display(
                &content,
                remaining.saturating_sub(suffix_width),
                Truncation::Right,
            );
            let suffix = truncate_display(&suffix, suffix_width, Truncation::Right);
            Line::from(vec![
                Span::styled(marker, marker_style),
                Span::styled(content, content_style),
                Span::styled(suffix, Style::default().fg(MUTED)),
            ])
        })
        .collect()
}

fn todo_summary(snapshot: &TodoRenderSnapshot, width: u16) -> Option<Line<'static>> {
    let item = snapshot.items.first()?;
    if width == 0 {
        return None;
    }
    let (marker, marker_style) = match item.status {
        ilar::todo::Status::Completed => ("✓ ", Style::default().fg(ASSISTANT)),
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

#[derive(Clone)]
struct StyledGrapheme {
    text: String,
    style: Style,
    width: usize,
}

fn wrap_styled_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return vec![Line::default()];
    }
    if line.width() <= width {
        return vec![line];
    }

    let cells: Vec<_> = line
        .spans
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
        .collect();
    if cells.is_empty() {
        return vec![Line::default()];
    }

    let mut output = Vec::new();
    let mut current = Vec::new();
    let mut current_width = 0usize;
    for cell in cells {
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

fn styled_line(cells: &[StyledGrapheme]) -> Line<'static> {
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

fn text_field_view(value: &str, width: u16) -> (String, u16) {
    text_field_view_at(value, value.len(), width)
}

fn text_field_view_at(value: &str, cursor: usize, width: u16) -> (String, u16) {
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

#[derive(Debug, PartialEq, Eq)]
enum PickerAction {
    Stay,
    Dismiss,
    Choose(String),
}

struct ModelPicker {
    models: Vec<&'static ilar::model::ModelInfo>,
    active_model: String,
    query: String,
    selected: usize,
    error: Option<String>,
}

impl ModelPicker {
    fn new(models: Vec<&'static ilar::model::ModelInfo>, active_model: &str) -> Self {
        let selected = models
            .iter()
            .position(|model| model.full_id() == active_model)
            .unwrap_or(0);
        Self {
            models,
            active_model: active_model.to_string(),
            query: String::new(),
            selected,
            error: None,
        }
    }

    fn filtered_models(&self) -> Vec<&'static ilar::model::ModelInfo> {
        let query = self.query.to_lowercase();
        let terms: Vec<_> = query.split_whitespace().collect();
        self.models
            .iter()
            .copied()
            .filter(|model| {
                let haystack =
                    format!("{} {} {}", model.provider, model.id, model.name).to_lowercase();
                terms.iter().all(|term| haystack.contains(term))
            })
            .collect()
    }

    #[cfg(test)]
    fn set_query(&mut self, query: &str) {
        self.query = query.to_string();
        self.selected = 0;
    }

    #[cfg(test)]
    fn selected_index(&self) -> usize {
        self.selected
    }

    fn move_selection(&mut self, delta: isize) {
        let count = self.filtered_models().len();
        if count == 0 {
            self.selected = 0;
        } else {
            self.selected = (self.selected as isize + delta).rem_euclid(count as isize) as usize;
        }
    }

    fn select_boundary(&mut self, end: bool) {
        self.selected = if end {
            self.filtered_models().len().saturating_sub(1)
        } else {
            0
        };
    }

    fn handle_key(&mut self, code: KeyCode, control: bool) -> PickerAction {
        match (code, control) {
            (KeyCode::Esc, _) => PickerAction::Dismiss,
            (KeyCode::Enter, _) => self
                .filtered_models()
                .get(self.selected)
                .map(|model| model.full_id())
                .map(|model| {
                    if model == self.active_model {
                        PickerAction::Dismiss
                    } else {
                        PickerAction::Choose(model)
                    }
                })
                .unwrap_or(PickerAction::Stay),
            (KeyCode::Up, _) | (KeyCode::Char('p'), true) => {
                self.move_selection(-1);
                PickerAction::Stay
            }
            (KeyCode::Down, _) | (KeyCode::Char('n'), true) => {
                self.move_selection(1);
                PickerAction::Stay
            }
            (KeyCode::PageUp, _) => {
                self.move_selection(-10);
                PickerAction::Stay
            }
            (KeyCode::PageDown, _) => {
                self.move_selection(10);
                PickerAction::Stay
            }
            (KeyCode::Home, _) => {
                self.select_boundary(false);
                PickerAction::Stay
            }
            (KeyCode::End, _) => {
                self.select_boundary(true);
                PickerAction::Stay
            }
            (KeyCode::Backspace, _) => {
                if let Some((index, _)) = self.query.grapheme_indices(true).next_back() {
                    self.query.truncate(index);
                }
                self.selected = 0;
                self.error = None;
                PickerAction::Stay
            }
            (KeyCode::Char(character), false) => {
                self.query.push(character);
                self.selected = 0;
                self.error = None;
                PickerAction::Stay
            }
            _ => PickerAction::Stay,
        }
    }
}

fn centered_rect(area: Rect, max_width: u16, max_height: u16) -> Rect {
    let width = max_width.min(area.width.saturating_sub(2).max(1));
    let height = max_height.min(area.height.saturating_sub(2).max(1));
    Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.y
            .saturating_add(area.height.saturating_sub(height) / 2),
        width,
        height,
    )
}

fn render_model_picker(frame: &mut Frame, picker: &ModelPicker) {
    let area = centered_rect(frame.area(), 78, 20);
    frame.render_widget(Clear, area);

    let footer = if area.width < 44 {
        " Enter select · Esc close "
    } else {
        " ↑↓ move · Enter select · Esc close "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" models ")
        .title_bottom(Line::from(footer).right_aligned());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let models = picker.filtered_models();
    let count = format!(
        "{}/{}",
        picker.selected.saturating_add(1).min(models.len()),
        models.len()
    );
    let fixed_width = 7usize
        .saturating_add(UnicodeWidthStr::width(count.as_str()))
        .saturating_add(1);
    let query_area_width = (inner.width as usize).saturating_sub(fixed_width) as u16;
    let (visible_query, query_cursor_offset) = text_field_view(&picker.query, query_area_width);
    let query = if picker.query.is_empty() {
        Span::styled(
            truncate_display(
                "type to filter",
                query_area_width as usize,
                Truncation::Right,
            ),
            Style::default().fg(MUTED),
        )
    } else {
        Span::raw(visible_query.clone())
    };
    let gap = (inner.width as usize)
        .saturating_sub(7)
        .saturating_sub(UnicodeWidthStr::width(query.content.as_ref()))
        .saturating_sub(UnicodeWidthStr::width(count.as_str()));
    let mut lines = vec![Line::from(vec![
        Span::styled("search ", Style::default().fg(MUTED)),
        query,
        Span::raw(" ".repeat(gap)),
        Span::styled(count, Style::default().fg(MUTED)),
    ])];
    if let Some(error) = &picker.error {
        let error = Line::styled(
            truncate_display(error, inner.width as usize, Truncation::Right),
            Style::default().fg(ERROR),
        );
        if inner.height >= 3 {
            lines.push(error);
        } else {
            lines[0] = error;
        }
    } else if inner.height >= 6 {
        lines.push(Line::styled(
            truncate_display(
                &format!("current {}", picker.active_model),
                inner.width as usize,
                Truncation::Middle,
            ),
            Style::default().fg(MUTED),
        ));
    }
    let row_count = inner.height.saturating_sub(lines.len() as u16) as usize;
    if models.is_empty() && row_count > 0 {
        lines.push(Line::styled(
            " no matching models",
            Style::default().fg(MUTED),
        ));
    } else if row_count > 0 {
        let selected = picker.selected.min(models.len().saturating_sub(1));
        let start = selected
            .saturating_add(1)
            .saturating_sub(row_count)
            .min(models.len().saturating_sub(row_count));
        for (index, model) in models.iter().enumerate().skip(start).take(row_count) {
            let full_id = model.full_id();
            let active = full_id == picker.active_model;
            let marker = if index == selected && active {
                ">●"
            } else if index == selected {
                "> "
            } else if active {
                " ●"
            } else {
                "  "
            };
            let context = format_tokens_compact(model.context_limit);
            let text = if inner.width >= 50 {
                let suffix = format!("  {full_id}  {context}");
                let name_width = (inner.width as usize)
                    .saturating_sub(UnicodeWidthStr::width(marker))
                    .saturating_sub(UnicodeWidthStr::width(suffix.as_str()))
                    .saturating_sub(2);
                format!(
                    "{marker} {}{suffix}",
                    truncate_display(model.name, name_width, Truncation::Right)
                )
            } else {
                format!("{marker} {full_id}")
            };
            let text = truncate_display(&text, inner.width as usize, Truncation::Right);
            let text = format!("{text:<width$}", width = inner.width as usize);
            let style = if index == selected {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else if active {
                Style::default().fg(ASSISTANT)
            } else {
                Style::default()
            };
            lines.push(Line::styled(text, style));
        }
    }

    frame.render_widget(Paragraph::new(lines), inner);
    let offset = 7usize
        .saturating_add(query_cursor_offset as usize)
        .min(inner.width.saturating_sub(1) as usize) as u16;
    frame.set_cursor_position((inner.x.saturating_add(offset), inner.y));
}

#[derive(Clone, Copy)]
enum Truncation {
    Right,
    Middle,
}

fn tool_line(
    name: &str,
    arguments: &str,
    state: ToolState,
    width: u16,
    elapsed: std::time::Duration,
) -> Line<'static> {
    let width = width as usize;
    let (state_icon, state_color) = match state {
        ToolState::Running => {
            let frames = ["◐", "◓", "◑", "◒"];
            (
                frames[(elapsed.as_millis() / 160) as usize % frames.len()],
                TOOL_ACTIVE,
            )
        }
        ToolState::Succeeded => ("✓", ASSISTANT),
        ToolState::Failed => ("×", ERROR),
    };
    let fixed = UnicodeWidthStr::width("tool ▶  ") + UnicodeWidthStr::width(state_icon);
    if width <= fixed {
        return Line::from(Span::styled(
            truncate_display(
                &format!("tool ▶ {name} {state_icon}"),
                width,
                Truncation::Right,
            ),
            Style::default().fg(TOOL_ACTIVE),
        ));
    }
    let name_budget = width.saturating_sub(fixed).clamp(1, 20);
    let name = truncate_display(name, name_budget, Truncation::Right);
    let used = fixed + UnicodeWidthStr::width(name.as_str());
    let arguments = truncate_display(
        &safe_text(arguments)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
        width.saturating_sub(used).saturating_sub(1),
        Truncation::Right,
    );
    let mut spans = vec![
        Span::styled("tool ", Style::default().fg(Color::Yellow)),
        Span::styled("▶ ", Style::default().fg(TOOL_ACTIVE)),
        Span::styled(name, Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(format!(" {state_icon}"), Style::default().fg(state_color)),
    ];
    if !arguments.is_empty() {
        spans.push(Span::styled(
            format!(" {arguments}"),
            Style::default().fg(MUTED),
        ));
    }
    Line::from(spans)
}

fn truncate_display(value: &str, max_width: usize, mode: Truncation) -> String {
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

fn abbreviated_path(path: &std::path::Path, home: Option<&std::path::Path>) -> String {
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

fn format_tokens_compact(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{}m", tokens / 1_000_000)
    } else if tokens >= 1_000 {
        format!("{}k", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

fn context_usage(used: u64, limit: Option<u64>, estimated: bool) -> String {
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

fn safe_text(text: &str) -> String {
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

fn safe_lines(text: &str) -> Vec<String> {
    let lines: Vec<_> = text.lines().map(safe_text).collect();
    if lines.is_empty() {
        vec![String::new()]
    } else {
        lines
    }
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
) -> Result<()> {
    drop(resolver.resolve_provider(model)?);
    let mut session = store.acquire_writer(session_id)?.load()?;
    session.append(ilar::session::SessionEvent::ModelChange {
        id: ilar::session::new_id(),
        model: model.to_string(),
        ts: chrono::Utc::now(),
    })?;
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

    let store = SessionStore::new(config.state_dir().join("sessions"));
    let resumed = args
        .session
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
    let agent_name = selected_agent_name(args.agent.as_deref(), persisted_agent.as_deref());
    let agents = config.agents().context("loading agent definitions")?;
    let agent = agents
        .iter()
        .find(|a| a.name == agent_name)
        .cloned()
        .with_context(|| format!("unknown agent {agent_name:?}"))?;
    let persisted_model = resumed.as_ref().map(|session| session.effective_model());
    let model_for_session = selected_model(
        args.model.as_deref(),
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
    let mut system_prompt = system_prompt_for(&cwd);
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

    let session_id = match &args.session {
        Some(id) => {
            if args.model.is_some()
                && persisted_model.as_deref() != Some(model_for_session.as_str())
            {
                persist_model_change(resolver.as_ref(), &store, id, &model_for_session)
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
        ..LoopConfig::default()
    };
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
        .with_background_tool_timeout(std::time::Duration::from_millis(
            config.subagents.background_tool_timeout_ms,
        ))
        .with_loop_config(loop_config.clone()),
    );
    let todos = std::sync::Arc::new(std::sync::Mutex::new(restored_todos(resumed.as_ref())));
    let registry = ToolRegistry::builtin()
        .with_subagents(spawner.clone())?
        .with_todos(todos.clone())?
        .with_web_tools()?
        .with_skills(skill_store)?;
    let notifications = spawner.subscribe();
    let tool_ctx = ToolContext::root(cwd.clone()).with_subagents(spawner.clone());
    let model_choices = config.available_models();

    let (context_used, context_estimated) =
        session_context_tokens(&store, &session_id, &system_prompt, &registry)?;
    let context_limit = resolver.context_limit(&model_for_session);
    let mut app = App::new();
    app.todos = todos;
    if let Some(resumed) = &resumed {
        app.restore_session(resumed);
    }
    app.configure_runtime(
        model_for_session.clone(),
        cwd.clone(),
        context_used,
        context_limit,
        context_estimated,
    );

    let (mut terminal, _terminal_session) = TerminalSession::start()?;
    run_app(
        &mut terminal,
        &mut app,
        resolver,
        &store,
        &session_id,
        &system_prompt,
        &registry,
        tool_ctx,
        spawner,
        notifications,
        loop_config,
        model_choices,
    )
    .await
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

#[allow(clippy::too_many_arguments)]
async fn run_app(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    resolver: Arc<dyn ProviderResolver>,
    store: &SessionStore,
    session_id: &str,
    system_prompt: &str,
    registry: &ToolRegistry,
    tool_ctx: ToolContext,
    spawner: std::sync::Arc<ilar::subagent::SubagentSpawner>,
    mut notifications: tokio::sync::mpsc::Receiver<ilar::subagent::Notification>,
    loop_config: LoopConfig,
    model_choices: Vec<&'static ilar::model::ModelInfo>,
) -> Result<()> {
    let mut events_rx: Option<LoopEventReceiver> = None;
    let mut pending_notification = None;
    let mut notifications_paused = false;
    let mut cancel: Option<CancellationToken> = None;
    let mut turn_handle: Option<tokio::task::JoinHandle<TurnCompletion>> = None;

    loop {
        // Drain pending loop events.
        if let Some(rx) = events_rx.as_mut() {
            while let Ok(event) = rx.try_recv() {
                app.push_loop_event(&event);
            }
        }
        // Turn finished? Join and clean up.
        if let Some(handle) = turn_handle.as_mut()
            && handle.is_finished()
        {
            let handle = turn_handle.take().unwrap();
            if let Some(rx) = events_rx.as_mut() {
                while let Ok(event) = rx.try_recv() {
                    app.push_loop_event(&event);
                }
            }
            match handle.await {
                Ok(TurnCompletion::Root(result)) => {
                    let aborted = matches!(result, Ok(TurnOutcome::Aborted));
                    app.finish_turn(result);
                    if !aborted {
                        notifications_paused = false;
                    }
                }
                Ok(TurnCompletion::Routed(Ok(ilar::subagent::RouteOutcome::Propagate(
                    notification,
                )))) => {
                    app.busy = false;
                    app.status = "ready".into();
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
                    app.set_activity(Activity::Ready);
                }
                Ok(TurnCompletion::Routed(Err(error))) => {
                    app.busy = false;
                    app.status = "error".into();
                    app.set_activity(Activity::Error);
                    app.lines.push(Line_::System(format!(
                        "notification routing failed: {error}"
                    )));
                }
                Err(error) => {
                    app.busy = false;
                    app.status = "error".into();
                    app.set_activity(Activity::Error);
                    app.lines.push(Line_::System(format!(
                        "notification routing failed: {error}"
                    )));
                }
            }
            events_rx = None;
            cancel = None;
        }

        // Background completions re-invoke their declared parent while idle.
        if let Some(notification) = next_notification(
            turn_handle.is_some(),
            notifications_paused || app.model_picker.is_some(),
            &mut pending_notification,
            &mut notifications,
        ) {
            if notification.parent_session_id != session_id {
                let token = CancellationToken::new();
                cancel = Some(token.clone());
                app.busy = true;
                app.status = format!("routing task to {}", notification.parent_session_id);
                app.set_activity(Activity::Thinking);
                let spawner = spawner.clone();
                turn_handle = Some(tokio::spawn(async move {
                    TurnCompletion::Routed(spawner.route_notification(notification, token).await)
                }));
                continue;
            }
            app.lines.push(Line_::System(format!(
                "task notification: {}",
                notification.description
            )));
            let text = notification.text;
            app.lines.push(Line_::User(text.clone()));
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

        terminal.draw(|frame| app.render(frame))?;

        // Poll terminal input (fast while busy so streaming keeps rendering).
        let timeout = if app.busy {
            std::time::Duration::from_millis(50)
        } else {
            std::time::Duration::from_millis(250)
        };
        if !crossterm::event::poll(timeout)? {
            continue;
        }
        match crossterm::event::read()? {
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
                    return Ok(());
                }
                if let Some(picker) = app.model_picker.as_mut() {
                    match picker.handle_key(code, control) {
                        PickerAction::Stay => {}
                        PickerAction::Dismiss => {
                            app.model_picker = None;
                            app.status = "ready".into();
                        }
                        PickerAction::Choose(new_model) => {
                            match persist_model_change(
                                resolver.as_ref(),
                                store,
                                session_id,
                                &new_model,
                            ) {
                                Ok(()) => {
                                    app.current_model = new_model.clone();
                                    app.context_limit = resolver.context_limit(&new_model);
                                    if let Ok((used, estimated)) = session_context_tokens(
                                        store,
                                        session_id,
                                        system_prompt,
                                        registry,
                                    ) {
                                        app.context_used = used;
                                        app.context_estimated = estimated;
                                    }
                                    app.status = "ready".into();
                                    app.lines
                                        .push(Line_::System(format!("switched to {new_model}")));
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
                if app.model_key_pending {
                    app.model_key_pending = false;
                    if code == KeyCode::Esc {
                        app.status = "ready".into();
                        continue;
                    }
                    if matches!(code, KeyCode::Char('m' | 'M'))
                        && !app.busy
                        && !model_choices.is_empty()
                    {
                        app.model_picker =
                            Some(ModelPicker::new(model_choices.clone(), &app.current_model));
                        continue;
                    }
                    app.status = "ready".into();
                }
                if matches!((code, control), (KeyCode::Char('x'), true)) && !app.busy {
                    app.model_key_pending = true;
                    app.status = "Ctrl-X: press M for models".into();
                    continue;
                }
                match (code, control) {
                    (KeyCode::Char('m'), true) | (KeyCode::F(2), false)
                        if !app.busy && !model_choices.is_empty() =>
                    {
                        app.model_picker =
                            Some(ModelPicker::new(model_choices.clone(), &app.current_model));
                    }
                    (KeyCode::Esc, _) => {
                        let background = spawner.running_background();
                        if background > 0 {
                            spawner.abort_all();
                            notifications_paused = true;
                            app.status = format!("cancelling {background} background job(s)…");
                            app.set_activity(if app.busy {
                                Activity::Aborting
                            } else {
                                Activity::Paused
                            });
                        }
                        if app.busy {
                            if let Some(cancel) = &cancel {
                                cancel.cancel();
                                app.status = "aborting…".into();
                                app.set_activity(Activity::Aborting);
                            }
                        } else if background == 0 {
                            app.input.clear();
                        }
                    }
                    (KeyCode::Char('u'), true) => {
                        app.scroll_up(app.page_size().div_ceil(2));
                    }
                    (KeyCode::Char('d'), true) => {
                        app.scroll_down(app.page_size().div_ceil(2));
                    }
                    (KeyCode::Home, true) => app.scroll_to_top(),
                    (KeyCode::End, true) => app.scroll_to_tail(),
                    (KeyCode::PageUp, _) => app.scroll_up(app.page_size()),
                    (KeyCode::PageDown, _) => app.scroll_down(app.page_size()),
                    (KeyCode::Up, _) if !app.input.is_multiline() => app.scroll_up(1),
                    (KeyCode::Down, _) if !app.input.is_multiline() => app.scroll_down(1),
                    _ => match handle_prompt_key(&mut app.input, key) {
                        PromptAction::Submit
                            if turn_handle.is_none() && !app.busy && !app.input.is_blank() =>
                        {
                            let text = app.input.take();
                            app.lines.push(Line_::User(text.clone()));
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
                        PromptAction::Edited | PromptAction::Unhandled | PromptAction::Submit => {}
                    },
                }
            }
            Event::Paste(text) if app.model_picker.is_none() => {
                app.model_key_pending = false;
                app.input.insert(&text);
            }
            Event::Mouse(mouse) if app.model_picker.is_none() => match mouse.kind {
                MouseEventKind::ScrollUp => app.scroll_up(3),
                MouseEventKind::ScrollDown => app.scroll_down(3),
                MouseEventKind::Down(MouseButton::Left) => {
                    app.begin_transcript_selection(mouse.column, mouse.row);
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    app.update_transcript_selection(mouse.column, mouse.row);
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    if let Some(text) = app.finish_transcript_selection(mouse.column, mouse.row)
                        && let Err(error) = app.copy_to_clipboard(&text)
                    {
                        app.lines
                            .push(Line_::System(format!("clipboard copy failed: {error:#}")));
                        app.follow_tail = true;
                        app.set_activity(Activity::Error);
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn resumed_session_restores_visible_events_and_latest_usage() {
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
                text: "remember this".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        let usage = ilar::session::Usage {
            input_tokens: 120,
            output_tokens: 30,
            cache_read_input_tokens: 40,
            cache_creation_input_tokens: 0,
            input_token_accounting: Some(ilar::session::InputTokenAccounting::ExcludesCached),
        };
        session
            .append(ilar::session::SessionEvent::AssistantMessage {
                id: new_id(),
                model: "zai/glm-4.7".into(),
                content: vec![
                    ilar::session::ContentBlock::Text {
                        text: "restored answer".into(),
                    },
                    ilar::session::ContentBlock::Thinking {
                        text: "hidden thought".into(),
                        signature: None,
                    },
                    ilar::session::ContentBlock::ToolCall {
                        id: "read-1".into(),
                        name: "read".into(),
                        input: Default::default(),
                    },
                ],
                usage,
                stop_reason: "tool_use".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::ToolResult {
                id: new_id(),
                tool_use_id: "read-1".into(),
                content: "file contents".into(),
                is_error: false,
                state: None,
                ts: chrono::Utc::now(),
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::ModelChange {
                id: new_id(),
                model: "openai/gpt-5.6-sol".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(session);

        let resumed = store.load(&session_id).unwrap();
        let view = restored_session_view(&resumed);
        assert_eq!(view.latest_usage, Some(usage));
        assert!(matches!(&view.lines[0], Line_::User(text) if text == "remember this"));
        assert!(matches!(&view.lines[1], Line_::Assistant(text) if text == "restored answer"));
        assert!(matches!(
            &view.lines[2],
            Line_::Tool { id, name, arguments, state: ToolState::Succeeded }
                if id == "read-1" && name == "read" && arguments.is_empty()
        ));
        assert!(matches!(
            view.lines.last(),
            Some(Line_::System(text)) if text.contains("openai/gpt-5.6-sol")
        ));
        let rendered = format!("{:?}", view.lines);
        assert!(!rendered.contains("hidden thought"), "{rendered}");
    }

    #[test]
    fn resumed_unfinished_tools_are_marked_failed() {
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
                    id: "unfinished".into(),
                    name: "bash".into(),
                    input: Default::default(),
                }],
                usage: ilar::session::Usage::default(),
                stop_reason: "tool_use".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(session);

        let view = restored_session_view(&store.load(&session_id).unwrap());
        assert!(matches!(
            view.lines.as_slice(),
            [Line_::Tool {
                state: ToolState::Failed,
                ..
            }]
        ));
    }

    #[test]
    fn resumed_compaction_replaces_old_history_with_the_summary() {
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
                text: "obsolete history".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::Compaction {
                id: new_id(),
                summary: "decisions retained here".into(),
                kept_from: 2,
                ts: chrono::Utc::now(),
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::UserMessage {
                id: new_id(),
                text: "current history".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(session);

        let view = restored_session_view(&store.load(&session_id).unwrap());
        let rendered = format!("{:?}", view.lines);
        assert!(!rendered.contains("obsolete history"), "{rendered}");
        assert!(rendered.contains("decisions retained here"), "{rendered}");
        assert!(rendered.contains("current history"), "{rendered}");
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
    fn idle_status_keeps_model_and_latest_step_usage() {
        let mut app = App::new();
        app.configure_runtime(
            "openai/gpt-5.6-sol".into(),
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

        let status = rendered_text(&app.status_line(120));
        assert!(status.contains("openai/gpt-5.6-sol"), "{status}");
        assert!(status.contains("in 300"), "{status}");
        assert!(status.contains("out 50"), "{status}");
        assert!(status.contains("cache 1520"), "{status}");
        let narrow = rendered_text(&app.status_line(60));
        assert!(narrow.contains("gpt-5.6"), "{narrow}");
        assert!(narrow.contains("i300/o50"), "{narrow}");
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
        assert!(persist_model_change(&resolver, &store, &session_id, "openai/gpt-5.2").is_err());
        assert_eq!(
            store.load(&session_id).unwrap().effective_model(),
            "zai/glm-4.7"
        );
        drop(writer);

        persist_model_change(&resolver, &store, &session_id, "openai/gpt-5.2").unwrap();
        assert_eq!(
            store.load(&session_id).unwrap().effective_model(),
            "openai/gpt-5.2"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn model_picker_searches_provider_id_and_display_name() {
        let models = ilar::model::catalog().iter().collect();
        let mut picker = ModelPicker::new(models, "openai/gpt-5.6-sol");

        picker.set_query("zai");
        assert!(
            picker
                .filtered_models()
                .iter()
                .all(|model| model.provider == "zai")
        );

        picker.set_query("glm-4.7");
        assert!(
            picker
                .filtered_models()
                .iter()
                .any(|model| model.full_id() == "zai/glm-4.7")
        );

        picker.set_query("GPT-5.6 Sol");
        assert_eq!(picker.filtered_models()[0].full_id(), "openai/gpt-5.6-sol");
    }

    #[test]
    fn model_picker_navigation_confirmation_and_escape_are_explicit() {
        let models = ilar::model::catalog().iter().take(3).collect();
        let mut picker = ModelPicker::new(models, "missing/model");

        assert_eq!(picker.selected_index(), 0);
        assert_eq!(picker.handle_key(KeyCode::Up, false), PickerAction::Stay);
        assert_eq!(picker.selected_index(), 2);
        assert!(matches!(
            picker.handle_key(KeyCode::Enter, false),
            PickerAction::Choose(_)
        ));
        assert_eq!(
            picker.handle_key(KeyCode::Esc, false),
            PickerAction::Dismiss
        );

        let active = ilar::model::catalog()[0].full_id();
        let mut picker = ModelPicker::new(vec![&ilar::model::catalog()[0]], &active);
        assert_eq!(
            picker.handle_key(KeyCode::Enter, false),
            PickerAction::Dismiss,
            "confirming the active model is a no-op"
        );
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
        assert_eq!(continuation_start, 6);
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
    fn styled_hard_wrap_preserves_code_and_never_adds_blank_rows() {
        let code = markdown::render("```\n    hello world\n```").remove(0);
        let original = rendered_text(&code);
        let wrapped = wrap_styled_line(code, 5);

        assert!(wrapped.iter().all(|line| !rendered_text(line).is_empty()));
        assert_eq!(
            wrapped.iter().map(rendered_text).collect::<String>(),
            original
        );
        assert!(
            wrapped
                .iter()
                .flat_map(|line| &line.spans)
                .all(|span| { span.style.fg == Some(Color::Cyan) })
        );

        let wide = wrap_styled_line(Line::raw("界界"), 2);
        assert_eq!(wide.len(), 2);
        assert!(wide.iter().all(|line| line.width() == 2));
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

    fn rendered_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
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
        });

        let lines = app.transcript_lines(36, std::time::Instant::now());
        let tool = lines.last().unwrap();
        assert!(UnicodeWidthStr::width(rendered_text(tool).as_str()) <= 36);
        assert_eq!(tool.spans.last().unwrap().style.fg, Some(MUTED));
        assert!(rendered_text(tool).contains("cargo test"));
        assert!(!rendered_text(tool).contains('\n'));
    }

    #[test]
    fn telemetry_always_contains_runtime_context() {
        let mut app = App::new();
        app.configure_runtime(
            "openai/gpt-5.6-sol".into(),
            std::path::PathBuf::from("/very/long/workspace/project"),
            68_000,
            Some(272_000),
            false,
        );
        let wide = rendered_text(&app.status_line(100));
        assert!(wide.contains("ready"));
        assert!(wide.contains("openai/gpt-5.6-sol"));
        assert!(wide.contains("project"));
        assert!(wide.contains("68.0k/272.0k"));
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
        assert!(rendered_text(tools.last().unwrap()).contains("running tools"));
        let first_tool = tool_line(
            "read",
            "src/main.rs",
            ToolState::Running,
            80,
            std::time::Duration::ZERO,
        );
        let next_tool = tool_line(
            "read",
            "src/main.rs",
            ToolState::Running,
            80,
            std::time::Duration::from_millis(200),
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
    fn tool_rows_never_exceed_their_width() {
        for width in 0..=100 {
            let line = tool_line(
                "extremely-long-tool-name",
                "👩‍💻 /very/long/path/to/a/file with arguments",
                ToolState::Succeeded,
                width,
                std::time::Duration::ZERO,
            );
            let rendered = rendered_text(&line);
            assert!(
                UnicodeWidthStr::width(rendered.as_str()) <= width as usize,
                "width {width}: {rendered:?}"
            );
        }
    }

    #[test]
    fn stopped_and_paused_states_remain_visible() {
        let mut app = App::new();
        app.push_loop_event(&LoopEvent::TurnDone {
            outcome: TurnOutcome::MaxIterations,
        });
        assert!(rendered_text(&app.status_line(80)).contains("stopped"));
        app.set_activity(Activity::Paused);
        assert!(rendered_text(&app.status_line(80)).contains("paused"));
    }

    #[test]
    fn narrow_terminal_keeps_transcript_status_and_input_visible() {
        let backend = ratatui::backend::TestBackend::new(40, 9);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.configure_runtime(
            "openai/gpt-5.6-sol".into(),
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
    fn wide_content_reserves_a_fixed_right_sidebar() {
        let wide = content_areas(Rect::new(0, 0, 80, 8));
        assert_eq!(wide.transcript, Rect::new(0, 0, 52, 8));
        assert_eq!(wide.todos, Some(Rect::new(52, 0, 28, 8)));

        let narrow = content_areas(Rect::new(0, 0, 79, 8));
        assert_eq!(narrow.transcript, Rect::new(0, 0, 79, 8));
        assert_eq!(narrow.todos, None);
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
        let backend = ratatui::backend::TestBackend::new(100, 12);
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
        assert_eq!(app.transcript_text_area.width, 69);

        app.begin_transcript_selection(80, 1);
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
