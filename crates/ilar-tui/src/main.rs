//! ilar TUI: transcript, streaming, tool display, input. Esc aborts.

mod diff;
mod markdown;
mod theme;

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
    Block, BorderType, Borders, Clear, Padding, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState,
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
#[allow(clippy::large_enum_variant)] // Tool entries own bounded detail and nested agent activity.
enum Line_ {
    User(String),
    Task(String),
    Job(String),
    Assistant(String),
    Thought {
        text: String,
        complete: bool,
    },
    Tool {
        id: String,
        group_id: String,
        name: String,
        kind: ToolKind,
        arguments: String,
        argument_detail: String,
        diff: Vec<diff::DiffLine>,
        result: Option<String>,
        state: ToolState,
        progress: ToolProgress,
        expanded: bool,
        full: bool,
        child_lines: Vec<Line_>,
        child_group: u64,
        child_running: bool,
        child_session_id: Option<String>,
    },
    System(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolState {
    Running,
    Complete,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ToolKind {
    Tool,
    Agent { name: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolProgress {
    None,
    Receiving {
        received_bytes: u64,
        last_data: std::time::Instant,
    },
    Queued,
    Executing {
        received_bytes: u64,
        started: std::time::Instant,
    },
}

#[derive(Default)]
struct TranscriptRenderCache {
    width: Option<u16>,
    revision: Option<u64>,
    entries: Vec<CachedTranscriptEntry>,
    #[cfg(test)]
    rebuilds: usize,
}

struct CachedTranscriptEntry {
    source: TranscriptEntry,
    rows: Vec<TranscriptRow>,
}

#[derive(Debug, Clone, PartialEq)]
enum TranscriptEntry {
    Item(Box<Line_>),
    ToolGroup {
        id: String,
        calls: Vec<Line_>,
        expanded: bool,
        child: bool,
    },
}

impl TranscriptEntry {
    fn is_child(&self) -> bool {
        matches!(self, Self::ToolGroup { child: true, .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TranscriptHitTarget {
    ToolGroup(String),
    Tool(String),
}

#[derive(Clone)]
struct TranscriptRow {
    line: Line<'static>,
    target: Option<TranscriptHitTarget>,
}

impl TranscriptRenderCache {
    fn update(
        &mut self,
        lines: &[Line_],
        expanded_groups: &std::collections::HashSet<String>,
        revision: u64,
        width: u16,
        now: std::time::Instant,
        activity_started: std::time::Instant,
    ) {
        if self.width != Some(width) {
            self.width = Some(width);
            self.revision = None;
            self.entries.clear();
        }
        if self.revision == Some(revision) {
            for (index, cached) in self.entries.iter_mut().enumerate() {
                if transcript_entry_animated(&cached.source) {
                    let mut rows = transcript_entry_rows(
                        &cached.source,
                        expanded_groups,
                        width,
                        now,
                        activity_started,
                        false,
                    );
                    if index > 0 && !cached.source.is_child() {
                        rows.insert(
                            0,
                            TranscriptRow {
                                line: Line::default(),
                                target: None,
                            },
                        );
                    }
                    cached.rows = rows;
                }
            }
            return;
        }
        let sources = transcript_entries(lines, expanded_groups);
        self.entries.truncate(sources.len());
        for (index, source) in sources.iter().enumerate() {
            let animated = transcript_entry_animated(source);
            let changed = self
                .entries
                .get(index)
                .is_none_or(|cached| cached.source != *source);
            if !changed && !animated {
                continue;
            }
            let mut rows =
                transcript_entry_rows(source, expanded_groups, width, now, activity_started, false);
            if index > 0 && !source.is_child() {
                rows.insert(
                    0,
                    TranscriptRow {
                        line: Line::default(),
                        target: None,
                    },
                );
            }
            if let Some(cached) = self.entries.get_mut(index) {
                if changed {
                    cached.source = source.clone();
                }
                cached.rows = rows;
            } else {
                self.entries.push(CachedTranscriptEntry {
                    source: source.clone(),
                    rows,
                });
            }
            #[cfg(test)]
            {
                self.rebuilds += 1;
            }
        }
        self.revision = Some(revision);
    }

    fn row_count(&self) -> usize {
        self.entries.iter().map(|entry| entry.rows.len()).sum()
    }

    fn visible_rows(
        &self,
        start: usize,
        count: usize,
        trailing: &[Line<'static>],
    ) -> Vec<TranscriptRow> {
        let mut skip = start;
        let mut remaining = count;
        let mut output = Vec::with_capacity(count.min(128));
        let trailing = trailing
            .iter()
            .cloned()
            .map(|line| TranscriptRow { line, target: None })
            .collect::<Vec<_>>();
        for rows in self
            .entries
            .iter()
            .map(|entry| entry.rows.as_slice())
            .chain(std::iter::once(trailing.as_slice()))
        {
            if remaining == 0 {
                break;
            }
            if skip >= rows.len() {
                skip -= rows.len();
                continue;
            }
            let available = rows.len() - skip;
            let take = available.min(remaining);
            output.extend(rows[skip..skip + take].iter().cloned());
            remaining -= take;
            skip = 0;
        }
        output
    }
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

fn task_notification_display(text: &str) -> Option<String> {
    notification_display(text, "task-notification", normalize_task_notification)
}

fn normalize_task_notification(first: &str) -> String {
    let Some(first) = first.strip_prefix("Task \"") else {
        return first.to_string();
    };
    for separator in [
        "\" completed.",
        "\" failed:",
        "\" was cancelled.",
        "\" was aborted.",
        "\" stalled:",
    ] {
        if let Some(index) = first.rfind(separator) {
            return format!("{} {}", &first[..index], &first[index + 2..]);
        }
    }
    format!("Task \"{first}")
}

fn tool_notification_display(text: &str) -> Option<String> {
    notification_display(text, "tool-notification", |first| {
        first
            .strip_prefix("Background job ")
            .unwrap_or(first)
            .to_string()
    })
}

fn notification_display(
    text: &str,
    tag: &str,
    normalize_first: impl FnOnce(&str) -> String,
) -> Option<String> {
    let opening = format!("<{tag}>\n");
    let closing = format!("\n</{tag}>");
    let inner = text.strip_prefix(&opening)?.strip_suffix(&closing)?;
    let (first, body) = inner.split_once('\n').unwrap_or((inner, ""));
    let body = body
        .strip_prefix("<result>\n")
        .and_then(|body| body.strip_suffix("\n</result>"))
        .unwrap_or(body);
    let first = normalize_first(first);
    if body.is_empty() {
        Some(first)
    } else {
        Some(format!("{first}\n{body}"))
    }
}

fn restored_session_view(session: &ilar::session::SessionReader) -> RestoredSessionView {
    restored_session_invocation_view(session, None)
}

fn restored_session_invocation_view(
    session: &ilar::session::SessionReader,
    parent_tool_call_id: Option<&str>,
) -> RestoredSessionView {
    let all_events = session.events();
    let events = match parent_tool_call_id {
        Some(parent_tool_call_id) => {
            let start = all_events.iter().position(|event| {
                matches!(
                    event,
                    ilar::session::SessionEvent::SubagentInvocation {
                        parent_tool_call_id: current,
                        ..
                    } if current == parent_tool_call_id
                )
            });
            let Some(start) = start else {
                return RestoredSessionView {
                    lines: Vec::new(),
                    latest_usage: None,
                };
            };
            let end = all_events[start + 1..]
                .iter()
                .position(|event| {
                    matches!(
                        event,
                        ilar::session::SessionEvent::SubagentInvocation { .. }
                    )
                })
                .map(|offset| start + 1 + offset)
                .unwrap_or(all_events.len());
            &all_events[start + 1..end]
        }
        None => all_events,
    };
    let mut cut = 0usize;
    let mut summary = None;
    for (index, event) in events.iter().enumerate() {
        if parent_tool_call_id.is_some() {
            continue;
        }
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
            ilar::session::SessionEvent::SubagentInvocation { .. } => {}
            ilar::session::SessionEvent::UserMessage { text, .. } => {
                match task_notification_display(text) {
                    Some(text) => lines.push(Line_::Task(text)),
                    None => match tool_notification_display(text) {
                        Some(text) => lines.push(Line_::Job(text)),
                        None => lines.push(Line_::User(text.clone())),
                    },
                }
            }
            ilar::session::SessionEvent::AssistantMessage {
                id: message_id,
                content,
                ..
            } => {
                let mut tool_run = 0usize;
                let mut in_tool_run = false;
                for block in content {
                    if matches!(block, ilar::session::ContentBlock::ToolCall { .. }) {
                        if !in_tool_run {
                            tool_run += 1;
                            in_tool_run = true;
                        }
                    } else {
                        in_tool_run = false;
                    }
                    match block {
                        ilar::session::ContentBlock::Text { text } => match lines.last_mut() {
                            Some(Line_::Assistant(current)) => current.push_str(text),
                            _ => lines.push(Line_::Assistant(text.clone())),
                        },
                        ilar::session::ContentBlock::ReasoningSummary {
                            text,
                            completed: true,
                        } => {
                            lines.push(Line_::Thought {
                                text: text.clone(),
                                complete: true,
                            });
                        }
                        ilar::session::ContentBlock::ReasoningSummary {
                            completed: false, ..
                        } => {}
                        ilar::session::ContentBlock::ToolCall { id, name, input } => {
                            let (kind, arguments) = if name == "task" {
                                match ilar::agent::summarize_task_input(input) {
                                    Some((description, agent)) => {
                                        (ToolKind::Agent { name: agent }, description)
                                    }
                                    None => (
                                        ToolKind::Agent {
                                            name: "subagent".into(),
                                        },
                                        ilar::agent::summarize_tool_input(name, input),
                                    ),
                                }
                            } else {
                                (
                                    ToolKind::Tool,
                                    ilar::agent::summarize_tool_input(name, input),
                                )
                            };
                            lines.push(Line_::Tool {
                                id: id.clone(),
                                group_id: format!("{message_id}:{tool_run}"),
                                name: name.clone(),
                                kind,
                                arguments,
                                argument_detail: ilar::agent::tool_argument_detail(name, input),
                                diff: diff::tool_diff_value(name, input),
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
                content,
                is_error,
                child_session_id,
                ..
            } => {
                if let Some((state, result, stored_child_session)) =
                    lines.iter_mut().rev().find_map(|line| match line {
                        Line_::Tool {
                            id,
                            state,
                            result,
                            child_session_id,
                            ..
                        } if id == tool_use_id => Some((state, result, child_session_id)),
                        _ => None,
                    })
                {
                    *state = if *is_error {
                        ToolState::Failed
                    } else {
                        ToolState::Succeeded
                    };
                    *result = Some(bounded_detail(content));
                    *stored_child_session = child_session_id.clone();
                }
            }
            ilar::session::SessionEvent::ModelChange { model, variant, .. } => {
                let selection = variant
                    .as_deref()
                    .map(|variant| format!("{model}@{variant}"))
                    .unwrap_or_else(|| model.clone());
                lines.push(Line_::System(format!("switched to {selection}")));
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

fn restored_session_view_with_store(
    session: &ilar::session::SessionReader,
    store: &SessionStore,
) -> RestoredSessionView {
    let mut view = restored_session_view(session);
    let owner_session_id = session
        .meta()
        .map(|meta| meta.session_id.as_str())
        .unwrap_or_default();
    restore_child_activity(&mut view.lines, store, owner_session_id, 0);
    view
}

fn restore_child_activity(
    lines: &mut [Line_],
    store: &SessionStore,
    owner_session_id: &str,
    depth: usize,
) {
    if depth >= 8 {
        return;
    }
    for line in lines {
        let Line_::Tool {
            id: parent_tool_call_id,
            child_session_id: Some(session_id),
            child_lines,
            ..
        } = line
        else {
            continue;
        };
        let Ok(session) = store.load(session_id) else {
            continue;
        };
        if session.meta().and_then(|meta| meta.parent_id.as_deref()) != Some(owner_session_id) {
            continue;
        }
        let mut restored =
            restored_session_invocation_view(&session, Some(parent_tool_call_id)).lines;
        if matches!(restored.first(), Some(Line_::User(_))) {
            restored.remove(0);
        }
        restore_child_activity(&mut restored, store, session_id, depth + 1);
        *child_lines = restored;
    }
}

fn reasoning_summary_title(summary: &str) -> String {
    let first = summary.trim_start().lines().next().unwrap_or("").trim();
    let title = first
        .strip_prefix("**")
        .and_then(|heading| heading.split_once("**").map(|(title, _)| title))
        .or_else(|| {
            first.starts_with('#').then(|| {
                first
                    .trim_start_matches('#')
                    .trim()
                    .trim_end_matches('#')
                    .trim()
            })
        })
        .unwrap_or_else(|| first.trim_matches('*'))
        .trim();
    if title.is_empty() {
        "reasoning".into()
    } else {
        safe_text(title)
    }
}

fn transcript_entries(
    lines: &[Line_],
    expanded_groups: &std::collections::HashSet<String>,
) -> Vec<TranscriptEntry> {
    let mut entries = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let Line_::Tool {
            id: first_call_id,
            group_id,
            kind: ToolKind::Tool,
            ..
        } = &lines[index]
        else {
            entries.push(TranscriptEntry::Item(Box::new(lines[index].clone())));
            index += 1;
            continue;
        };
        let start = index;
        while index < lines.len()
            && matches!(
                &lines[index],
                Line_::Tool {
                    kind: ToolKind::Tool,
                    ..
                }
            )
        {
            index += 1;
        }
        let group_id = format!("{group_id}:{first_call_id}");
        entries.push(TranscriptEntry::ToolGroup {
            id: group_id.clone(),
            calls: lines[start..index].to_vec(),
            expanded: expanded_groups.contains(&group_id),
            child: start > 0 && matches!(lines[start - 1], Line_::Thought { .. }),
        });
    }
    entries
}

fn toggle_tool_expansion(lines: &mut [Line_], id: &str) -> bool {
    for line in lines {
        if let Line_::Tool {
            id: line_id,
            expanded,
            full,
            child_lines,
            ..
        } = line
        {
            if line_id == id {
                match (*expanded, *full) {
                    (false, _) => *expanded = true,
                    (true, false) => *full = true,
                    (true, true) => {
                        *expanded = false;
                        *full = false;
                    }
                }
                return true;
            }
            if toggle_tool_expansion(child_lines, id) {
                return true;
            }
        }
    }
    false
}

fn contains_child_session(lines: &[Line_], session_id: &str) -> bool {
    lines.iter().any(|line| match line {
        Line_::Tool {
            child_session_id,
            child_lines,
            ..
        } => {
            child_session_id.as_deref() == Some(session_id)
                || contains_child_session(child_lines, session_id)
        }
        _ => false,
    })
}

fn session_lines_for_call_mut<'a>(
    lines: &'a mut [Line_],
    session_id: &str,
    call_id: &str,
) -> Option<&'a mut Vec<Line_>> {
    for line in lines {
        if let Line_::Tool {
            child_session_id,
            child_lines,
            ..
        } = line
        {
            if child_session_id.as_deref() == Some(session_id)
                && child_lines
                    .iter()
                    .any(|line| matches!(line, Line_::Tool { id, .. } if id == call_id))
            {
                return Some(child_lines);
            }
            if let Some(found) = session_lines_for_call_mut(child_lines, session_id, call_id) {
                return Some(found);
            }
        }
    }
    None
}

fn direct_tool_mut<'a>(lines: &'a mut [Line_], id: &str) -> Option<&'a mut Line_> {
    lines
        .iter_mut()
        .find(|line| matches!(line, Line_::Tool { id: line_id, .. } if line_id == id))
}

fn apply_subagent_activity(
    lines: &mut Vec<Line_>,
    root_session_id: &str,
    activity: &ilar::subagent::SubagentActivity,
) -> bool {
    let owner = if activity.parent_session_id == root_session_id || root_session_id.is_empty() {
        lines
    } else if contains_child_session(lines, &activity.parent_session_id) {
        let Some(owner) = session_lines_for_call_mut(
            lines,
            &activity.parent_session_id,
            &activity.parent_call_id,
        ) else {
            return false;
        };
        owner
    } else {
        return false;
    };
    let Some(Line_::Tool {
        child_lines,
        child_group,
        child_running,
        child_session_id,
        ..
    }) = direct_tool_mut(owner, &activity.parent_call_id)
    else {
        return false;
    };
    *child_session_id = Some(activity.child_session_id.clone());
    *child_running = !matches!(activity.event, LoopEvent::TurnDone { .. });
    apply_child_loop_event(
        child_lines,
        child_group,
        &activity.parent_call_id,
        &activity.event,
    );
    true
}

fn apply_child_loop_event(lines: &mut Vec<Line_>, group: &mut u64, scope: &str, event: &LoopEvent) {
    match event {
        LoopEvent::TextDelta(text) => match lines.last_mut() {
            Some(Line_::Assistant(current)) => current.push_str(text),
            _ => lines.push(Line_::Assistant(text.clone())),
        },
        LoopEvent::ThinkingDelta(_) => {
            if !matches!(
                lines.last(),
                Some(Line_::Thought {
                    complete: false,
                    ..
                })
            ) {
                lines.push(Line_::Thought {
                    text: "reasoning".into(),
                    complete: false,
                });
            }
        }
        LoopEvent::ReasoningSummaryDelta(summary) => match lines.last_mut() {
            Some(Line_::Thought {
                text,
                complete: false,
            }) => text.push_str(summary),
            _ => lines.push(Line_::Thought {
                text: summary.clone(),
                complete: false,
            }),
        },
        LoopEvent::ReasoningSummaryCompleted => {
            if let Some(complete) = lines.iter_mut().rev().find_map(|line| match line {
                Line_::Thought { complete, .. } if !*complete => Some(complete),
                _ => None,
            }) {
                *complete = true;
            }
        }
        LoopEvent::ToolStarted { id, name } => lines.push(Line_::Tool {
            id: id.clone(),
            group_id: format!("{scope}:{group}"),
            name: name.clone(),
            kind: ToolKind::Tool,
            arguments: String::new(),
            argument_detail: String::new(),
            diff: Vec::new(),
            result: None,
            state: ToolState::Running,
            progress: ToolProgress::None,
            expanded: false,
            full: false,
            child_lines: Vec::new(),
            child_group: 0,
            child_running: false,
            child_session_id: None,
        }),
        LoopEvent::ToolArguments { id, arguments } => {
            if let Some(Line_::Tool {
                arguments: current, ..
            }) = direct_tool_mut(lines, id)
            {
                *current = arguments.clone();
            }
        }
        LoopEvent::ToolInputProgress {
            id,
            received_bytes,
            last_data,
        } => {
            if let Some(Line_::Tool {
                state: ToolState::Running,
                progress,
                ..
            }) = direct_tool_mut(lines, id)
                && !matches!(
                    progress,
                    ToolProgress::Queued | ToolProgress::Executing { .. }
                )
            {
                *progress = ToolProgress::Receiving {
                    received_bytes: *received_bytes,
                    last_data: *last_data,
                };
            }
        }
        LoopEvent::ToolInputComplete { id, arguments } => {
            if let Some(Line_::Tool {
                name,
                progress,
                argument_detail,
                diff,
                ..
            }) = direct_tool_mut(lines, id)
            {
                *progress = ToolProgress::Queued;
                *argument_detail = bounded_detail(arguments);
                *diff = diff::tool_diff(name, arguments);
            }
        }
        LoopEvent::SubagentConfigured {
            id,
            description,
            agent,
        } => {
            if let Some(Line_::Tool {
                kind, arguments, ..
            }) = direct_tool_mut(lines, id)
            {
                *kind = ToolKind::Agent {
                    name: agent.clone(),
                };
                *arguments = description.clone();
            }
        }
        LoopEvent::ToolExecutionStarted {
            id,
            received_bytes,
            started,
        } => {
            if let Some(Line_::Tool { progress, .. }) = direct_tool_mut(lines, id) {
                *progress = ToolProgress::Executing {
                    received_bytes: *received_bytes,
                    started: *started,
                };
            }
        }
        LoopEvent::ToolExecutionCompleted { id } => {
            if let Some(Line_::Tool {
                state, progress, ..
            }) = direct_tool_mut(lines, id)
            {
                *state = ToolState::Complete;
                *progress = ToolProgress::None;
            }
        }
        LoopEvent::ToolFinished {
            id,
            is_error,
            result,
            child_session_id,
            ..
        } => {
            if let Some(Line_::Tool {
                state,
                progress,
                result: current,
                child_session_id: current_child,
                ..
            }) = direct_tool_mut(lines, id)
            {
                *state = if *is_error {
                    ToolState::Failed
                } else {
                    ToolState::Succeeded
                };
                *progress = ToolProgress::None;
                *current = Some(bounded_detail(result));
                *current_child = child_session_id.clone();
            }
        }
        LoopEvent::StepComplete { .. } => *group = group.saturating_add(1),
        LoopEvent::TurnDone { outcome } => {
            lines.retain(|line| {
                !matches!(
                    line,
                    Line_::Thought {
                        complete: false,
                        ..
                    }
                )
            });
            if *outcome != TurnOutcome::Completed {
                mark_running_tools_failed(lines);
            }
        }
        LoopEvent::TurnStarted | LoopEvent::Compacted { .. } => {}
    }
}

fn mark_running_tools_failed(lines: &mut [Line_]) {
    for line in lines {
        if let Line_::Tool {
            state, child_lines, ..
        } = line
        {
            if matches!(*state, ToolState::Running | ToolState::Complete) {
                *state = ToolState::Failed;
            }
            mark_running_tools_failed(child_lines);
        }
    }
}

fn transcript_entry_animated(entry: &TranscriptEntry) -> bool {
    match entry {
        TranscriptEntry::Item(item) => tool_is_active(item),
        TranscriptEntry::ToolGroup { calls, .. } => calls.iter().any(tool_is_active),
    }
}

fn tool_is_active(line: &Line_) -> bool {
    matches!(
        line,
        Line_::Tool {
            state: ToolState::Running | ToolState::Complete,
            ..
        } | Line_::Tool {
            child_running: true,
            ..
        }
    )
}

fn transcript_entry_rows(
    entry: &TranscriptEntry,
    expanded_groups: &std::collections::HashSet<String>,
    width: u16,
    now: std::time::Instant,
    activity_started: std::time::Instant,
    nested: bool,
) -> Vec<TranscriptRow> {
    match entry {
        TranscriptEntry::Item(item) => match item.as_ref() {
            tool @ Line_::Tool { .. } => {
                tool_entry_rows(tool, expanded_groups, width, now, activity_started, 0, None)
            }
            item => transcript_entry_lines(item, width, now, activity_started)
                .into_iter()
                .flat_map(|line| wrap_styled_line(line, width as usize))
                .map(|line| TranscriptRow { line, target: None })
                .collect(),
        },
        TranscriptEntry::ToolGroup {
            id,
            calls,
            expanded,
            child,
        } => {
            let running = calls.iter().filter(|call| tool_is_active(call)).count();
            let failed = calls
                .iter()
                .filter(|call| {
                    matches!(
                        call,
                        Line_::Tool {
                            state: ToolState::Failed,
                            ..
                        }
                    )
                })
                .count();
            let show_hierarchy = width >= 64;
            let group_indent = if *child && show_hierarchy && !nested {
                2
            } else {
                0
            };
            let mut header = tool_group_line(
                calls.len(),
                running,
                failed,
                *expanded,
                width.saturating_sub(group_indent as u16),
            );
            if group_indent > 0 {
                let mut spans = vec![Span::styled(
                    hierarchy_prefix(group_indent, "└─"),
                    Style::default().fg(theme::BORDER),
                )];
                spans.append(&mut header.spans);
                header = Line::from(spans);
            }
            let mut rows = vec![TranscriptRow {
                line: header,
                target: Some(TranscriptHitTarget::ToolGroup(id.clone())),
            }];
            let visible = calls
                .iter()
                .filter(|call| *expanded || tool_is_active(call))
                .collect::<Vec<_>>();
            let visible_count = visible.len();
            for (index, call) in visible.into_iter().enumerate() {
                let branch = show_hierarchy.then_some(if index + 1 == visible_count {
                    "└─"
                } else {
                    "├─"
                });
                let call_indent = if show_hierarchy { group_indent + 2 } else { 0 };
                rows.extend(tool_entry_rows(
                    call,
                    expanded_groups,
                    width,
                    now,
                    activity_started,
                    call_indent,
                    branch,
                ));
            }
            rows
        }
    }
}

fn tool_group_line(
    calls: usize,
    running: usize,
    failed: usize,
    expanded: bool,
    width: u16,
) -> Line<'static> {
    let disclosure = if expanded { "▾" } else { "▸" };
    let (status, icon, color) = if running > 0 {
        (
            format!("{running} running · {}", call_count(calls)),
            "◐",
            TOOL_ACTIVE,
        )
    } else if failed > 0 {
        (
            format!("{} · {failed} failed", call_count(calls)),
            "×",
            ERROR,
        )
    } else {
        (call_count(calls), "✓", theme::SUCCESS)
    };
    let text = truncate_display(
        &format!("tools {disclosure} {status} {icon}"),
        width as usize,
        Truncation::Right,
    );
    Line::from(Span::styled(text, Style::default().fg(color)))
}

fn call_count(calls: usize) -> String {
    format!("{calls} {}", if calls == 1 { "call" } else { "calls" })
}

fn tool_entry_rows(
    entry: &Line_,
    expanded_groups: &std::collections::HashSet<String>,
    width: u16,
    now: std::time::Instant,
    activity_started: std::time::Instant,
    indent: usize,
    branch: Option<&str>,
) -> Vec<TranscriptRow> {
    let indent = indent.min(width as usize);
    let Line_::Tool {
        id,
        name,
        kind,
        arguments,
        argument_detail,
        diff,
        result,
        state,
        progress,
        expanded,
        full,
        child_lines,
        child_running,
        ..
    } = entry
    else {
        return Vec::new();
    };
    let display_state = if *child_running {
        ToolState::Running
    } else {
        *state
    };
    let line = tool_line_with_disclosure(
        name,
        kind,
        arguments,
        display_state,
        width.saturating_sub(indent as u16),
        now.saturating_duration_since(activity_started),
        *progress,
        now,
        *expanded,
        *full,
    );
    let mut spans = branch
        .map(|branch| {
            vec![Span::styled(
                hierarchy_prefix(indent, branch),
                Style::default().fg(theme::BORDER),
            )]
        })
        .unwrap_or_default();
    spans.extend(line.spans);
    let mut rows = vec![TranscriptRow {
        line: Line::from(spans),
        target: Some(TranscriptHitTarget::Tool(id.clone())),
    }];
    if *expanded {
        if diff.is_empty() {
            rows.extend(tool_detail_rows(
                "args",
                argument_detail,
                width,
                indent + 4,
                if *full { usize::MAX } else { 4 },
                false,
            ));
        } else {
            rows.extend(tool_diff_rows(
                diff,
                width,
                indent + 4,
                if *full { usize::MAX } else { 8 },
            ));
        }
        if matches!(kind, ToolKind::Tool) || child_lines.is_empty() || *state == ToolState::Failed {
            rows.extend(tool_detail_rows(
                "result",
                result.as_deref().unwrap_or("pending"),
                width,
                indent + 4,
                if *full { usize::MAX } else { 8 },
                *state == ToolState::Failed,
            ));
        }
    }
    if matches!(kind, ToolKind::Agent { .. })
        && (*expanded
            || *child_running
            || matches!(*state, ToolState::Running | ToolState::Complete))
    {
        let nested_indent = if width >= 64 {
            (indent + 4).min(width as usize)
        } else {
            0
        };
        let visible = if *expanded {
            child_lines.clone()
        } else {
            agent_live_preview(child_lines)
        };
        let entries = transcript_entries(&visible, expanded_groups);
        let entry_count = entries.len();
        for (index, child) in entries.into_iter().enumerate() {
            let last = index + 1 == entry_count;
            let child_rows = transcript_entry_rows(
                &child,
                expanded_groups,
                width.saturating_sub(nested_indent as u16),
                now,
                activity_started,
                true,
            );
            rows.extend(child_rows.into_iter().enumerate().map(|(row_index, row)| {
                let branch = if row_index == 0 {
                    if last { "└─" } else { "├─" }
                } else if last {
                    "  "
                } else {
                    "│ "
                };
                indent_transcript_row(row, nested_indent, branch)
            }));
        }
    }
    rows
}

fn agent_live_preview(lines: &[Line_]) -> Vec<Line_> {
    let mut preview = if let Some(response @ Line_::Assistant(_)) = lines.last() {
        vec![response.clone()]
    } else {
        lines
            .iter()
            .rfind(|line| matches!(line, Line_::Thought { .. }))
            .cloned()
            .into_iter()
            .collect::<Vec<_>>()
    };
    preview.extend(lines.iter().filter(|line| tool_is_active(line)).cloned());
    if preview.is_empty() {
        preview.push(Line_::System("thinking…".into()));
    }
    preview
}

fn indent_transcript_row(mut row: TranscriptRow, indent: usize, branch: &str) -> TranscriptRow {
    let mut spans = vec![Span::styled(
        hierarchy_prefix(indent, branch),
        Style::default().fg(theme::BORDER),
    )];
    spans.append(&mut row.line.spans);
    row.line = Line::from(spans);
    row
}

fn hierarchy_prefix(indent: usize, branch: &str) -> String {
    if indent < 2 {
        return " ".repeat(indent);
    }
    format!("{}{branch}", " ".repeat(indent - 2))
}

/// Indent/label/content column split shared by the labeled detail rows.
struct DetailLayout {
    indent: usize,
    label_width: usize,
    content_width: usize,
}

fn detail_layout(width: usize, indent: usize) -> DetailLayout {
    let indent = indent.min(width.saturating_sub(1));
    let remaining = width - indent;
    let label_width = 8usize.min(remaining.saturating_sub(1));
    DetailLayout {
        indent,
        label_width,
        content_width: remaining.saturating_sub(label_width).max(1),
    }
}

fn labeled_rows(
    label: &str,
    mut content: Vec<Line<'static>>,
    layout: &DetailLayout,
    limit: usize,
    error: bool,
) -> Vec<TranscriptRow> {
    let truncated = content.len() > limit;
    content.truncate(limit);
    if truncated && let Some(last) = content.last_mut() {
        *last = Line::styled(
            truncate_display("… more", layout.content_width, Truncation::Right),
            Style::default().fg(MUTED),
        );
    }
    if content.is_empty() {
        content.push(Line::default());
    }
    let label_width = layout.label_width;
    let label_style = Style::default().fg(if error { ERROR } else { MUTED });
    content
        .into_iter()
        .enumerate()
        .map(|(index, mut line)| {
            let mut spans = vec![
                Span::raw(" ".repeat(layout.indent)),
                Span::styled(
                    if index == 0 {
                        format!(
                            "{:<label_width$}",
                            truncate_display(label, label_width, Truncation::Right)
                        )
                    } else {
                        " ".repeat(label_width)
                    },
                    label_style,
                ),
            ];
            spans.append(&mut line.spans);
            TranscriptRow {
                line: Line::from(spans),
                target: None,
            }
        })
        .collect()
}

fn tool_detail_rows(
    label: &str,
    text: &str,
    width: u16,
    indent: usize,
    limit: usize,
    error: bool,
) -> Vec<TranscriptRow> {
    let width = width as usize;
    if width == 0 {
        return vec![TranscriptRow {
            line: Line::default(),
            target: None,
        }];
    }
    let layout = detail_layout(width, indent);
    let content = safe_lines(text)
        .into_iter()
        .flat_map(|line| wrap_styled_line(Line::raw(line), layout.content_width))
        .collect::<Vec<_>>();
    labeled_rows(label, content, &layout, limit, error)
}

fn tool_diff_rows(
    diff: &[diff::DiffLine],
    width: u16,
    indent: usize,
    limit: usize,
) -> Vec<TranscriptRow> {
    let width = width as usize;
    if width == 0 {
        return vec![TranscriptRow {
            line: Line::default(),
            target: None,
        }];
    }
    let layout = detail_layout(width, indent);
    let content = diff
        .iter()
        .flat_map(|line| {
            let (marker, color) = match line.kind {
                diff::DiffKind::Added => ("+", theme::SUCCESS),
                diff::DiffKind::Removed => ("-", ERROR),
                diff::DiffKind::Context => (" ", MUTED),
            };
            wrap_styled_line(
                Line::from(Span::styled(
                    format!("{marker} {}", safe_text(&line.text)),
                    Style::default().fg(color),
                )),
                layout.content_width,
            )
        })
        .collect::<Vec<_>>();
    labeled_rows("diff", content, &layout, limit, false)
}

fn transcript_entry_lines(
    entry: &Line_,
    width: u16,
    now: std::time::Instant,
    activity_started: std::time::Instant,
) -> Vec<Line<'static>> {
    match entry {
        Line_::Assistant(text) => {
            let mut output = Vec::new();
            let mut first = true;
            let label_width = 5usize.min(width.saturating_sub(2) as usize);
            let content_width = (width as usize).saturating_sub(label_width);
            for line in markdown::render(text, content_width) {
                if line.spans.is_empty() {
                    output.push(Line::default());
                    continue;
                }
                for mut line in wrap_markdown_line(line, content_width) {
                    for span in &mut line.spans {
                        if span.style.fg.is_none() {
                            span.style = span.style.fg(theme::PRIMARY);
                        }
                    }
                    let label = if first {
                        truncate_display("ilar ", label_width, Truncation::Right)
                    } else {
                        " ".repeat(label_width)
                    };
                    first = false;
                    let mut spans = vec![Span::styled(label, theme::title(theme::ASSISTANT))];
                    spans.append(&mut line.spans);
                    output.push(Line::from(spans));
                }
            }
            output
        }
        Line_::Thought { text, complete } => {
            let state = if *complete { "Thought" } else { "Thinking" };
            let title = reasoning_summary_title(text);
            vec![Line::from(Span::styled(
                truncate_display(
                    &format!("+ {state}: {title}"),
                    width as usize,
                    Truncation::Right,
                ),
                Style::default().fg(theme::REASONING),
            ))]
        }
        Line_::User(text) => safe_lines(text)
            .into_iter()
            .enumerate()
            .map(|(index, text)| {
                Line::from(vec![
                    Span::styled(
                        if index == 0 { "you  " } else { "     " },
                        theme::title(theme::USER),
                    ),
                    Span::styled(text, Style::default().fg(theme::PRIMARY)),
                ])
            })
            .collect(),
        Line_::Task(text) => safe_lines(text)
            .into_iter()
            .enumerate()
            .map(|(index, text)| {
                Line::from(vec![
                    Span::styled(
                        if index == 0 { "task " } else { "     " },
                        theme::title(theme::REASONING),
                    ),
                    Span::styled(text, Style::default().fg(theme::PRIMARY)),
                ])
            })
            .collect(),
        Line_::Job(text) => safe_lines(text)
            .into_iter()
            .enumerate()
            .map(|(index, text)| {
                Line::from(vec![
                    Span::styled(
                        if index == 0 { "job  " } else { "     " },
                        theme::title(theme::WAITING),
                    ),
                    Span::styled(text, Style::default().fg(theme::PRIMARY)),
                ])
            })
            .collect(),
        Line_::Tool {
            name,
            kind,
            arguments,
            state,
            progress,
            ..
        } => vec![tool_line(
            name,
            kind,
            arguments,
            *state,
            width,
            now.saturating_duration_since(activity_started),
            *progress,
            now,
        )],
        Line_::System(text) => safe_lines(text)
            .into_iter()
            .enumerate()
            .map(|(index, text)| {
                Line::from(vec![
                    Span::styled(
                        if index == 0 { "—    " } else { "     " },
                        Style::default().fg(theme::MUTED),
                    ),
                    Span::styled(text, Style::default().fg(theme::MUTED)),
                ])
            })
            .collect(),
    }
}

fn activity_line(
    busy: bool,
    activity: Activity,
    now: std::time::Instant,
    activity_started: std::time::Instant,
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
    theme: theme::ThemeId,
    theme_picker: Option<ThemePicker>,
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
            scroll_top: 0,
            content_rows: 0,
            viewport_rows: 0,
            follow_tail: true,
            command_palette: None,
            model_picker: None,
            variant_picker: None,
            session_picker: None,
            session_picker_requested: false,
            theme: theme::ThemeId::Terminal,
            theme_picker: None,
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
            self.command_palette = Some(CommandPalette::new());
        }
    }

    fn has_modal(&self) -> bool {
        self.command_palette.is_some()
            || self.model_picker.is_some()
            || self.variant_picker.is_some()
            || self.theme_picker.is_some()
            || self.session_picker.is_some()
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
        self.transcript_revision = self.transcript_revision.wrapping_add(1);
    }

    fn set_activity(&mut self, activity: Activity) {
        if self.activity != activity {
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
            self.lines.push(Line_::Task(text));
        } else if let Some(text) = tool_notification_display(text) {
            self.lines.push(Line_::Job(text));
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
            LoopEvent::ReasoningSummaryDelta(summary) => {
                self.status = "thinking".into();
                self.set_activity(Activity::Thinking);
                match self.lines.last_mut() {
                    Some(Line_::Thought {
                        text,
                        complete: false,
                    }) => text.push_str(summary),
                    _ => self.lines.push(Line_::Thought {
                        text: summary.clone(),
                        complete: false,
                    }),
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
                self.lines.push(Line_::Tool {
                    id: id.clone(),
                    group_id: format!("live:{}", self.next_tool_group),
                    name: name.clone(),
                    kind: ToolKind::Tool,
                    arguments: String::new(),
                    argument_detail: String::new(),
                    diff: Vec::new(),
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
                self.next_tool_group = self.next_tool_group.saturating_add(1);
                self.latest_usage = Some(*usage);
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
            LoopEvent::Compacted { context_tokens } => {
                self.context_used = *context_tokens;
                self.context_estimated = true;
                self.lines
                    .push(Line_::System("transcript compacted".into()));
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
            let message = format!("error: {error:#}");
            self.set_notice(&message, NoticeLevel::Error);
            self.lines.push(Line_::System(message));
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
        if let Some(activity) = activity_line(self.busy, self.activity, now, self.activity_started)
        {
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
        let compact_latest_usage = self.latest_usage.map(|latest| {
            format!(
                "i{}/o{} req-cache r{}/w{} {percent}",
                format_tokens_compact(latest.input_tokens),
                format_tokens_compact(latest.output_tokens),
                format_tokens_compact(latest.cache_read_input_tokens),
                format_tokens_compact(latest.cache_creation_input_tokens)
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
        let detailed_usage = self.latest_usage.map(|latest| {
            format!(
                "in {} · out {} · req cache r{}/w{} · {context_display}",
                latest.input_tokens,
                latest.output_tokens,
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
        let mut activity_rows = activity_line(self.busy, self.activity, now, self.activity_started)
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
        let text = visible.into_iter().map(|row| row.line).collect::<Vec<_>>();
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

        let input_focused = !self.busy && !self.has_modal();
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
        let input_title = if input_view.line_count > 1 {
            format!(
                " input {}/{} ",
                input_view.cursor_line, input_view.line_count
            )
        } else {
            " input ".into()
        };
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

        if !self.busy && !self.has_modal() && input_area.width > 0 && input_area.height > 0 {
            frame.set_cursor_position((
                input_area.x.saturating_add(input_view.cursor_x),
                input_area.y.saturating_add(input_view.cursor_y),
            ));
        }

        if let Some(picker) = &self.model_picker {
            render_model_picker(frame, picker);
        } else if let Some(picker) = &self.variant_picker {
            render_variant_picker(frame, picker);
        } else if let Some(picker) = &self.theme_picker {
            render_theme_picker(frame, picker);
        } else if let Some(picker) = &self.session_picker {
            render_session_picker(frame, picker);
        } else if let Some(palette) = &self.command_palette {
            render_command_palette(frame, palette);
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

#[derive(Clone)]
struct StyledGrapheme {
    text: String,
    style: Style,
    width: usize,
}

fn wrap_markdown_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
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

fn wrap_styled_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
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

fn styled_graphemes(line: Line<'static>) -> Vec<StyledGrapheme> {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaletteCommand {
    Model,
    Reasoning,
    Theme,
    Session,
}

#[derive(Debug, PartialEq, Eq)]
struct PaletteCommandDefinition {
    id: PaletteCommand,
    section: &'static str,
    label: &'static str,
    shortcut: &'static str,
    search_terms: &'static str,
}

static PALETTE_COMMANDS: &[PaletteCommandDefinition] = &[
    PaletteCommandDefinition {
        id: PaletteCommand::Model,
        section: "General",
        label: "Switch model",
        shortcut: "F2",
        search_terms: "model provider",
    },
    PaletteCommandDefinition {
        id: PaletteCommand::Reasoning,
        section: "General",
        label: "Switch reasoning",
        shortcut: "",
        search_terms: "variant thinking effort level",
    },
    PaletteCommandDefinition {
        id: PaletteCommand::Theme,
        section: "General",
        label: "Switch theme",
        shortcut: "F3",
        search_terms: "theme appearance colors palette",
    },
    PaletteCommandDefinition {
        id: PaletteCommand::Session,
        section: "General",
        label: "Resume session",
        shortcut: "",
        search_terms: "session resume continue switch history recent",
    },
];

#[derive(Debug, PartialEq, Eq)]
enum CommandPaletteAction {
    Stay,
    Dismiss,
    Choose(PaletteCommand),
}

struct CommandPalette {
    query: String,
    selected: usize,
}

impl CommandPalette {
    fn new() -> Self {
        Self {
            query: String::new(),
            selected: 0,
        }
    }

    fn filtered_commands(&self) -> Vec<&'static PaletteCommandDefinition> {
        let query = self.query.to_lowercase();
        let terms = query.split_whitespace().collect::<Vec<_>>();
        PALETTE_COMMANDS
            .iter()
            .filter(|command| {
                let haystack = format!(
                    "{} {} {} {}",
                    command.section, command.label, command.shortcut, command.search_terms
                )
                .to_lowercase();
                terms.iter().all(|term| haystack.contains(term))
            })
            .collect()
    }

    fn move_selection(&mut self, delta: isize) {
        let count = self.filtered_commands().len();
        if count == 0 {
            self.selected = 0;
        } else {
            self.selected = (self.selected as isize + delta).rem_euclid(count as isize) as usize;
        }
    }

    fn insert_query(&mut self, text: &str) {
        self.query
            .extend(text.chars().filter(|character| !character.is_control()));
        self.selected = 0;
    }

    fn handle_key(&mut self, code: KeyCode, control: bool) -> CommandPaletteAction {
        match (code, control) {
            (KeyCode::Esc, _) => CommandPaletteAction::Dismiss,
            (KeyCode::Enter, _) => self
                .filtered_commands()
                .get(self.selected)
                .map(|command| CommandPaletteAction::Choose(command.id))
                .unwrap_or(CommandPaletteAction::Stay),
            (KeyCode::Up, _) | (KeyCode::Char('p'), true) => {
                self.move_selection(-1);
                CommandPaletteAction::Stay
            }
            (KeyCode::Down, _) | (KeyCode::Char('n'), true) => {
                self.move_selection(1);
                CommandPaletteAction::Stay
            }
            (KeyCode::Home, _) => {
                self.selected = 0;
                CommandPaletteAction::Stay
            }
            (KeyCode::End, _) => {
                self.selected = self.filtered_commands().len().saturating_sub(1);
                CommandPaletteAction::Stay
            }
            (KeyCode::Backspace, _) => {
                if let Some((index, _)) = self.query.grapheme_indices(true).next_back() {
                    self.query.truncate(index);
                }
                self.selected = 0;
                CommandPaletteAction::Stay
            }
            (KeyCode::Char(character), false) => {
                self.insert_query(&character.to_string());
                CommandPaletteAction::Stay
            }
            _ => CommandPaletteAction::Stay,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum PickerAction {
    Stay,
    Dismiss,
    Choose(String),
}

struct SessionPicker {
    sessions: Vec<ilar::session::SessionSummary>,
    selected: usize,
}

impl SessionPicker {
    fn new(sessions: Vec<ilar::session::SessionSummary>) -> Self {
        Self {
            sessions,
            selected: 0,
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let count = self.sessions.len();
        if count == 0 {
            self.selected = 0;
        } else {
            self.selected = (self.selected as isize + delta).rem_euclid(count as isize) as usize;
        }
    }

    fn handle_key(&mut self, code: KeyCode, control: bool) -> PickerAction {
        match (code, control) {
            (KeyCode::Esc, _) => PickerAction::Dismiss,
            (KeyCode::Enter, _) => self
                .sessions
                .get(self.selected)
                .map(|session| PickerAction::Choose(session.id.clone()))
                .unwrap_or(PickerAction::Dismiss),
            (KeyCode::Up, _) | (KeyCode::Char('p'), true) => {
                self.move_selection(-1);
                PickerAction::Stay
            }
            (KeyCode::Down, _) | (KeyCode::Char('n'), true) => {
                self.move_selection(1);
                PickerAction::Stay
            }
            (KeyCode::Home, _) => {
                self.selected = 0;
                PickerAction::Stay
            }
            (KeyCode::End, _) => {
                self.selected = self.sessions.len().saturating_sub(1);
                PickerAction::Stay
            }
            _ => PickerAction::Stay,
        }
    }
}

fn session_age(modified: std::time::SystemTime, now: std::time::SystemTime) -> String {
    let seconds = now.duration_since(modified).unwrap_or_default().as_secs();
    match seconds {
        0..=59 => "now".into(),
        60..=3_599 => format!("{}m", seconds / 60),
        3_600..=86_399 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", seconds / 86_400),
    }
}

fn render_session_picker(frame: &mut Frame, picker: &SessionPicker) {
    let area = centered_rect(frame.area(), 72, 14);
    frame.render_widget(Clear, area);
    let footer = if area.width < 40 {
        " ↵ resume · Esc "
    } else {
        " ↑↓ select · Enter resume · Esc cancel "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(theme::focus_border())
        .title(Line::styled(" sessions ", theme::title(theme::MARKUP)))
        .title_bottom(Line::styled(footer, Style::default().fg(theme::MUTED)).right_aligned());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    if picker.sessions.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "no other sessions",
                Style::default().fg(MUTED),
            )),
            inner,
        );
        return;
    }
    let now = std::time::SystemTime::now();
    let selected = picker.selected.min(picker.sessions.len() - 1);
    let row_count = inner.height as usize;
    let start = selected
        .saturating_add(1)
        .saturating_sub(row_count)
        .min(picker.sessions.len().saturating_sub(row_count));
    let mut lines = Vec::new();
    for (index, session) in picker
        .sessions
        .iter()
        .enumerate()
        .skip(start)
        .take(row_count)
    {
        let marker = if index == selected { "> " } else { "  " };
        let age = session_age(session.modified, now);
        let title = session.title.as_deref().unwrap_or("(no messages yet)");
        let label_width = (inner.width as usize)
            .saturating_sub(UnicodeWidthStr::width(marker))
            .saturating_sub(UnicodeWidthStr::width(age.as_str()))
            .saturating_sub(1);
        let label = truncate_display(title, label_width, Truncation::Right);
        let text = format!(
            "{marker}{label:<label_width$} {age}",
            label_width = label_width
        );
        let text = truncate_display(&text, inner.width as usize, Truncation::Right);
        let style = if index == selected {
            theme::selected()
        } else {
            Style::default().fg(theme::PRIMARY)
        };
        lines.push(Line::styled(
            format!("{text:<width$}", width = inner.width as usize),
            style,
        ));
    }
    frame.render_widget(Paragraph::new(lines), inner);
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
                .map(|model| {
                    let id = model.full_id();
                    if id == self.active_model && model.variants().is_empty() {
                        PickerAction::Dismiss
                    } else {
                        PickerAction::Choose(id)
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

#[derive(Debug, PartialEq, Eq)]
enum VariantPickerAction {
    Stay,
    Dismiss,
    Choose(Option<String>),
}

struct VariantPicker {
    model: &'static ilar::model::ModelInfo,
    active_variant: Option<String>,
    selected: usize,
    error: Option<String>,
}

impl VariantPicker {
    fn new(model: &'static ilar::model::ModelInfo, active_variant: Option<&str>) -> Self {
        let selected = active_variant
            .and_then(|active| {
                model
                    .variants()
                    .iter()
                    .position(|variant| variant.id == active)
            })
            .map(|index| index + 1)
            .unwrap_or(0);
        Self {
            model,
            active_variant: active_variant.map(String::from),
            selected,
            error: None,
        }
    }

    #[cfg(test)]
    fn selected_index(&self) -> usize {
        self.selected
    }

    fn move_selection(&mut self, delta: isize) {
        let count = self.model.variants().len() + 1;
        self.selected = (self.selected as isize + delta).rem_euclid(count as isize) as usize;
        self.error = None;
    }

    fn selected_variant(&self) -> Option<String> {
        self.selected
            .checked_sub(1)
            .and_then(|index| self.model.variants().get(index))
            .map(|variant| variant.id.to_string())
    }

    fn handle_key(&mut self, code: KeyCode, control: bool) -> VariantPickerAction {
        match (code, control) {
            (KeyCode::Esc, _) => VariantPickerAction::Dismiss,
            (KeyCode::Enter, _) => {
                let selected = self.selected_variant();
                if selected == self.active_variant {
                    VariantPickerAction::Dismiss
                } else {
                    VariantPickerAction::Choose(selected)
                }
            }
            (KeyCode::Up, _) | (KeyCode::Char('p'), true) => {
                self.move_selection(-1);
                VariantPickerAction::Stay
            }
            (KeyCode::Down, _) | (KeyCode::Char('n'), true) => {
                self.move_selection(1);
                VariantPickerAction::Stay
            }
            (KeyCode::Home, _) => {
                self.selected = 0;
                VariantPickerAction::Stay
            }
            (KeyCode::End, _) => {
                self.selected = self.model.variants().len();
                VariantPickerAction::Stay
            }
            _ => VariantPickerAction::Stay,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThemePickerAction {
    Preview(theme::ThemeId),
    Dismiss,
    Choose(theme::ThemeId),
}

struct ThemePicker {
    active_theme: theme::ThemeId,
    selected: usize,
    error: Option<String>,
}

impl ThemePicker {
    fn new(active_theme: theme::ThemeId) -> Self {
        let selected = theme::ThemeId::ALL
            .iter()
            .position(|candidate| *candidate == active_theme)
            .unwrap_or_default();
        Self {
            active_theme,
            selected,
            error: None,
        }
    }

    fn selected_theme(&self) -> theme::ThemeId {
        theme::ThemeId::ALL[self.selected.min(theme::ThemeId::ALL.len() - 1)]
    }

    fn select(&mut self, selected: usize) -> ThemePickerAction {
        self.selected = selected.min(theme::ThemeId::ALL.len() - 1);
        self.error = None;
        ThemePickerAction::Preview(self.selected_theme())
    }

    fn move_selection(&mut self, delta: isize) -> ThemePickerAction {
        let count = theme::ThemeId::ALL.len() as isize;
        self.selected = (self.selected as isize + delta).rem_euclid(count) as usize;
        self.error = None;
        ThemePickerAction::Preview(self.selected_theme())
    }

    fn handle_key(&mut self, code: KeyCode, control: bool) -> ThemePickerAction {
        match (code, control) {
            (KeyCode::Esc, _) => ThemePickerAction::Dismiss,
            (KeyCode::Enter, _) => ThemePickerAction::Choose(self.selected_theme()),
            (KeyCode::Up, _) | (KeyCode::Char('p'), true) => self.move_selection(-1),
            (KeyCode::Down, _) | (KeyCode::Char('n'), true) => self.move_selection(1),
            (KeyCode::Home, _) => self.select(0),
            (KeyCode::End, _) => self.select(theme::ThemeId::ALL.len() - 1),
            _ => ThemePickerAction::Preview(self.selected_theme()),
        }
    }
}

fn apply_theme_picker_action(
    app: &mut App,
    action: ThemePickerAction,
    persist: impl FnOnce(theme::ThemeId) -> Result<ilar::config::ThemePersistOutcome>,
) {
    match action {
        ThemePickerAction::Preview(preview) => app.theme = preview,
        ThemePickerAction::Dismiss => {
            if let Some(picker) = app.theme_picker.take() {
                app.theme = picker.active_theme;
            }
            app.status = "ready".into();
            app.clear_transient_notice();
        }
        ThemePickerAction::Choose(selected) => match persist(selected) {
            Ok(outcome) => {
                app.theme = selected;
                app.theme_picker = None;
                app.status = format!("theme: {}", selected.label());
                match outcome {
                    ilar::config::ThemePersistOutcome::Saved => app.set_notice(
                        format!("theme saved: {}", selected.label()),
                        NoticeLevel::Info,
                    ),
                    ilar::config::ThemePersistOutcome::DurabilityUncertain(error) => app
                        .set_notice(
                            format!("theme updated, but durability is uncertain: {error}"),
                            NoticeLevel::Warning,
                        ),
                }
            }
            Err(error) => {
                if let Some(picker) = app.theme_picker.as_mut() {
                    picker.error = Some(format!("cannot save theme: {error}"));
                }
            }
        },
    }
}

fn activate_palette_command(
    app: &mut App,
    command: PaletteCommand,
    model_choices: Vec<&'static ilar::model::ModelInfo>,
) {
    app.command_palette = None;
    match command {
        PaletteCommand::Model if !model_choices.is_empty() => {
            app.model_picker = Some(ModelPicker::new(model_choices, &app.current_model));
        }
        PaletteCommand::Model => {}
        PaletteCommand::Reasoning => {
            if let Some(model) = ilar::model::find(&app.current_model)
                && !model.variants().is_empty()
            {
                app.variant_picker =
                    Some(VariantPicker::new(model, app.current_variant.as_deref()));
            } else {
                app.status = "current model has no reasoning variants".into();
                app.set_notice(
                    "current model has no reasoning variants",
                    NoticeLevel::Warning,
                );
            }
        }
        PaletteCommand::Theme => {
            app.theme_picker = Some(ThemePicker::new(app.theme));
        }
        PaletteCommand::Session => {
            // Sessions are loaded by the caller (needs the store); the
            // palette only records the request.
            app.session_picker_requested = true;
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

fn render_command_palette(frame: &mut Frame, palette: &CommandPalette) {
    let area = centered_rect(frame.area(), 72, 8);
    frame.render_widget(Clear, area);

    let footer = if area.width < 44 {
        " Enter select · Esc close "
    } else {
        " ↑↓ move · Enter select · Esc close "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(theme::focus_border())
        .title(Line::styled(" commands ", theme::title(theme::PRIMARY)))
        .title_bottom(Line::styled(footer, Style::default().fg(theme::MUTED)).right_aligned());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let query_area_width = inner.width.saturating_sub(7);
    let (visible_query, query_cursor_offset) = text_field_view(&palette.query, query_area_width);
    let query = if palette.query.is_empty() {
        Span::styled(
            truncate_display(
                "type to search commands",
                query_area_width as usize,
                Truncation::Right,
            ),
            Style::default().fg(MUTED),
        )
    } else {
        Span::raw(visible_query)
    };
    let mut lines = vec![Line::from(vec![
        Span::styled("search ", Style::default().fg(MUTED)),
        query,
    ])];
    let commands = palette.filtered_commands();
    if commands.is_empty() {
        if inner.height > 1 {
            lines.push(Line::styled(
                " no matching commands",
                Style::default().fg(MUTED),
            ));
        }
    } else {
        if inner.height >= 4 {
            lines.push(Line::default());
        }
        if inner.height >= 3 {
            lines.push(Line::styled(
                commands[0].section,
                Style::default()
                    .fg(TOOL_ACTIVE)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        let row_count = inner.height.saturating_sub(lines.len() as u16) as usize;
        let selected = palette.selected.min(commands.len().saturating_sub(1));
        let start = selected
            .saturating_add(1)
            .saturating_sub(row_count)
            .min(commands.len().saturating_sub(row_count));
        for (index, command) in commands.iter().enumerate().skip(start).take(row_count) {
            let marker = if index == selected { "> " } else { "  " };
            let shortcut =
                (inner.width >= 32 && !command.shortcut.is_empty()).then_some(command.shortcut);
            let suffix_width = shortcut
                .map(|shortcut| UnicodeWidthStr::width(shortcut).saturating_add(1))
                .unwrap_or(0);
            let label_width = (inner.width as usize)
                .saturating_sub(UnicodeWidthStr::width(marker))
                .saturating_sub(suffix_width);
            let label = truncate_display(command.label, label_width, Truncation::Right);
            let gap = shortcut
                .map(|shortcut| {
                    " ".repeat(
                        (inner.width as usize)
                            .saturating_sub(UnicodeWidthStr::width(marker))
                            .saturating_sub(UnicodeWidthStr::width(label.as_str()))
                            .saturating_sub(UnicodeWidthStr::width(shortcut)),
                    )
                })
                .unwrap_or_default();
            let text = shortcut
                .map(|shortcut| format!("{marker}{label}{gap}{shortcut}"))
                .unwrap_or_else(|| format!("{marker}{label}"));
            let text = format!("{text:<width$}", width = inner.width as usize);
            let style = if index == selected {
                theme::selected()
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

fn render_variant_picker(frame: &mut Frame, picker: &VariantPicker) {
    let area = centered_rect(frame.area(), 54, 10);
    frame.render_widget(Clear, area);

    let footer = if area.width < 38 {
        " Enter select · Esc close "
    } else {
        " ↑↓ move · Enter select · Esc close "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(theme::focus_border())
        .title(Line::styled(" reasoning ", theme::title(theme::REASONING)))
        .title_bottom(Line::styled(footer, Style::default().fg(theme::MUTED)).right_aligned());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let mut lines = Vec::new();
    if let Some(error) = &picker.error {
        lines.push(Line::styled(
            truncate_display(error, inner.width as usize, Truncation::Right),
            Style::default().fg(ERROR),
        ));
    } else if inner.height >= 6 {
        lines.push(Line::styled(
            truncate_display(picker.model.name, inner.width as usize, Truncation::Right),
            Style::default().fg(MUTED),
        ));
    }

    let row_count = inner.height.saturating_sub(lines.len() as u16) as usize;
    let choice_count = picker.model.variants().len() + 1;
    let selected = picker.selected.min(choice_count.saturating_sub(1));
    let start = selected
        .saturating_add(1)
        .saturating_sub(row_count)
        .min(choice_count.saturating_sub(row_count));
    for index in start..choice_count.min(start.saturating_add(row_count)) {
        let (id, name) = if index == 0 {
            ("default", "Provider default")
        } else {
            let variant = &picker.model.variants()[index - 1];
            (variant.id, variant.name)
        };
        let active = picker.active_variant.as_deref() == (index > 0).then_some(id);
        let marker = if index == selected && active {
            ">●"
        } else if index == selected {
            "> "
        } else if active {
            " ●"
        } else {
            "  "
        };
        let suffix = format!("  {id}");
        let name_width = (inner.width as usize)
            .saturating_sub(UnicodeWidthStr::width(marker))
            .saturating_sub(UnicodeWidthStr::width(suffix.as_str()))
            .saturating_sub(1);
        let text = format!(
            "{marker} {}{suffix}",
            truncate_display(name, name_width, Truncation::Right)
        );
        let text = truncate_display(&text, inner.width as usize, Truncation::Right);
        let text = format!("{text:<width$}", width = inner.width as usize);
        let style = if index == selected {
            theme::selected()
        } else if active {
            Style::default().fg(theme::SUCCESS)
        } else {
            Style::default()
        };
        lines.push(Line::styled(text, style));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_theme_picker(frame: &mut Frame, picker: &ThemePicker) {
    let area = centered_rect(frame.area(), 58, 12);
    frame.render_widget(Clear, area);

    let footer = if area.width < 32 {
        " ↵ save · Esc undo "
    } else if area.width < 48 {
        " Enter save · Esc undo "
    } else {
        " ↑↓ preview · Enter save · Esc undo · saved "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(theme::focus_border())
        .title(Line::styled(" themes ", theme::title(theme::MARKUP)))
        .title_bottom(Line::styled(footer, Style::default().fg(theme::MUTED)).right_aligned());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let selected = picker.selected.min(theme::ThemeId::ALL.len() - 1);
    let mut lines = Vec::new();
    if let Some(error) = &picker.error {
        lines.push(Line::styled(
            truncate_display(error, inner.width as usize, Truncation::Right),
            Style::default().fg(ERROR),
        ));
    } else {
        lines.push(Line::styled(
            truncate_display(
                theme::ThemeId::ALL[selected].description(),
                inner.width as usize,
                Truncation::Right,
            ),
            Style::default().fg(MUTED),
        ));
    }

    let show_sample = inner.height as usize > theme::ThemeId::ALL.len() + 1;
    let row_count = inner
        .height
        .saturating_sub(lines.len() as u16)
        .saturating_sub(u16::from(show_sample)) as usize;
    let start = selected
        .saturating_add(1)
        .saturating_sub(row_count)
        .min(theme::ThemeId::ALL.len().saturating_sub(row_count));
    for (index, choice) in theme::ThemeId::ALL
        .iter()
        .enumerate()
        .skip(start)
        .take(row_count)
    {
        let active = *choice == picker.active_theme;
        let marker = if index == selected { "> " } else { "  " };
        let suffix = if active {
            "  saved".to_string()
        } else if inner.width >= 34 {
            format!("  {}", choice.id())
        } else {
            String::new()
        };
        let label_width = (inner.width as usize)
            .saturating_sub(UnicodeWidthStr::width(marker))
            .saturating_sub(UnicodeWidthStr::width(suffix.as_str()))
            .saturating_sub(1);
        let text = format!(
            "{marker} {}{suffix}",
            truncate_display(choice.label(), label_width, Truncation::Right)
        );
        let text = truncate_display(&text, inner.width as usize, Truncation::Right);
        let text = format!("{text:<width$}", width = inner.width as usize);
        lines.push(Line::styled(
            text,
            if index == selected {
                theme::selected()
            } else if active {
                Style::default().fg(theme::SUCCESS)
            } else {
                Style::default()
            },
        ));
    }
    if show_sample {
        lines.push(Line::from(vec![
            Span::styled("you ", theme::title(theme::USER)),
            Span::styled("ilar ", theme::title(theme::ASSISTANT)),
            Span::styled("thought ", Style::default().fg(theme::REASONING)),
            Span::styled("tool ", Style::default().fg(theme::RUNNING)),
            Span::styled("✓ ", Style::default().fg(theme::SUCCESS)),
            Span::styled("×", Style::default().fg(theme::ERROR)),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), inner);
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
        .border_type(BorderType::Double)
        .border_style(theme::focus_border())
        .title(Line::styled(" models ", theme::title(theme::PRIMARY)))
        .title_bottom(Line::styled(footer, Style::default().fg(theme::MUTED)).right_aligned());
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
                theme::selected()
            } else if active {
                Style::default().fg(theme::SUCCESS)
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

#[allow(clippy::too_many_arguments)]
fn tool_line(
    name: &str,
    kind: &ToolKind,
    arguments: &str,
    state: ToolState,
    width: u16,
    elapsed: std::time::Duration,
    progress: ToolProgress,
    now: std::time::Instant,
) -> Line<'static> {
    tool_line_with_disclosure(
        name, kind, arguments, state, width, elapsed, progress, now, false, false,
    )
}

#[allow(clippy::too_many_arguments)]
fn tool_line_with_disclosure(
    name: &str,
    kind: &ToolKind,
    arguments: &str,
    state: ToolState,
    width: u16,
    elapsed: std::time::Duration,
    progress: ToolProgress,
    now: std::time::Instant,
    expanded: bool,
    full: bool,
) -> Line<'static> {
    let width = width as usize;
    let tool_name = name;
    let arguments = safe_text(arguments)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let (label, name, label_color) = match kind {
        ToolKind::Tool => ("tool", tool_name.to_string(), theme::SECONDARY),
        ToolKind::Agent { name } => ("agent", name.clone(), theme::REASONING),
    };
    let label = if width >= 72 {
        format!("{label:<6}")
    } else {
        format!("{label} ")
    };
    let (state_icon, state_color) = match state {
        ToolState::Running => {
            let frames = ["◐", "◓", "◑", "◒"];
            (
                frames[(elapsed.as_millis() / 160) as usize % frames.len()],
                TOOL_ACTIVE,
            )
        }
        ToolState::Complete => ("•", theme::SECONDARY),
        ToolState::Succeeded => ("✓", theme::SUCCESS),
        ToolState::Failed => ("×", ERROR),
    };
    let disclosure = match (expanded, full) {
        (false, _) => "▶",
        (true, false) => "▾",
        (true, true) => "▼",
    };
    let fixed = UnicodeWidthStr::width(format!("{label}{disclosure}  ").as_str())
        + UnicodeWidthStr::width(state_icon);
    if width <= fixed {
        return Line::from(Span::styled(
            truncate_display(
                &format!("{label}{disclosure} {name} {state_icon}"),
                width,
                Truncation::Right,
            ),
            Style::default().fg(label_color),
        ));
    }
    let progress = match (state, progress) {
        (
            ToolState::Running,
            ToolProgress::Receiving {
                received_bytes,
                last_data,
            },
        ) => {
            let quiet = now.saturating_duration_since(last_data);
            if quiet >= std::time::Duration::from_secs(2) {
                format!(
                    "waiting for provider · {} received · last data {}s ago",
                    format_bytes(received_bytes),
                    quiet.as_secs()
                )
            } else {
                format!("receiving {}", format_bytes(received_bytes))
            }
        }
        (ToolState::Running, ToolProgress::Queued) => "queued".into(),
        (
            ToolState::Running,
            ToolProgress::Executing {
                received_bytes,
                started,
            },
        ) => {
            let elapsed = format_elapsed(now.saturating_duration_since(started));
            if tool_name == "task" {
                format!("running · {elapsed}")
            } else if tool_name == "write" && received_bytes > 0 {
                format!("writing {} · {elapsed}", format_bytes(received_bytes))
            } else if tool_name == "write" {
                format!("writing · {elapsed}")
            } else {
                format!("executing · {elapsed}")
            }
        }
        (ToolState::Complete, _) => "done".into(),
        _ => String::new(),
    };
    let progress_reserve = progress
        .split_whitespace()
        .next()
        .map(|label| UnicodeWidthStr::width(label) + 2)
        .unwrap_or(0);
    let available_name = width.saturating_sub(fixed).saturating_sub(progress_reserve);
    let name_column = available_name.clamp(1, 20);
    let name = truncate_display(&name, name_column, Truncation::Right);
    let name_padding = if width >= 72 {
        name_column.saturating_sub(UnicodeWidthStr::width(name.as_str()))
    } else {
        0
    };
    let used = fixed + UnicodeWidthStr::width(name.as_str()) + name_padding;
    let details_color = if progress.starts_with("waiting") || progress == "queued" {
        theme::WAITING
    } else {
        theme::SECONDARY
    };
    let details = match (arguments.is_empty(), progress.is_empty()) {
        (false, false) => format!("{progress} · {arguments}"),
        (false, true) => arguments,
        (true, false) => progress,
        (true, true) => String::new(),
    };
    let details = truncate_display(
        &details,
        width.saturating_sub(used).saturating_sub(1),
        Truncation::Right,
    );
    let mut spans = vec![
        Span::styled(label, Style::default().fg(label_color)),
        Span::styled(
            format!("{disclosure} "),
            Style::default().fg(theme::SECONDARY),
        ),
        Span::styled(
            format!("{name}{}", " ".repeat(name_padding)),
            Style::default()
                .fg(theme::PRIMARY)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {state_icon}"), Style::default().fg(state_color)),
    ];
    if !details.is_empty() {
        spans.push(Span::styled(
            format!(" {details}"),
            Style::default().fg(details_color),
        ));
    }
    Line::from(spans)
}

fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn format_elapsed(duration: std::time::Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m {}s", seconds / 60, seconds % 60)
    }
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

fn context_meter(used: u64, limit: Option<u64>, estimated: bool, cells: usize) -> Option<String> {
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

fn bounded_detail(text: &str) -> String {
    const MAX_DETAIL_CHARS: usize = 16 * 1024;
    let mut detail = text.lines().map(safe_text).collect::<Vec<_>>().join("\n");
    if detail.chars().count() > MAX_DETAIL_CHARS {
        detail = detail.chars().take(MAX_DETAIL_CHARS).collect();
        detail.push_str("\n… output truncated");
    }
    detail
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
    app.context_limit = resolver.context_limit(&model);
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
            .with_user_config_dir(config.dirs().0.to_path_buf())
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
        let subagent_activity = spawner.subscribe_activity();
        let tool_ctx = ToolContext::root(cwd.clone()).with_subagents(spawner.clone());
        let model_choices = config.available_models();
        let user_config_path = config.dirs().0.join("ilar.toml");

        let (context_used, context_estimated) =
            session_context_tokens(&store, &session_id, &system_prompt, &registry)?;
        let context_limit = resolver.context_limit(&model_for_session);
        let mut app = App::new();
        app.theme = active_theme;
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
        )
        .await?;
        active_theme = app.theme;
        match exit {
            AppExit::Quit => return Ok(()),
            AppExit::Switch(next) => session_override = Some(next),
        }
    } // session loop
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

fn is_command_palette_shortcut(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(KeyEvent {
            code: KeyCode::Char('p'),
            kind: KeyEventKind::Press | KeyEventKind::Repeat,
            modifiers,
            ..
        }) if modifiers.contains(KeyModifiers::CONTROL)
    )
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
                if app.theme_picker.is_some() {
                    let action = {
                        let picker = app.theme_picker.as_mut().unwrap();
                        picker.handle_key(code, control)
                    };
                    apply_theme_picker_action(app, action, |selected| {
                        ilar::config::persist_general_theme(user_config_path, selected.id())
                    });
                    continue;
                }
                if let Some(picker) = app.session_picker.as_mut() {
                    match picker.handle_key(code, control) {
                        PickerAction::Stay => {}
                        PickerAction::Dismiss => {
                            app.session_picker = None;
                            app.clear_transient_notice();
                        }
                        PickerAction::Choose(new_session) => {
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
                if let Some(picker) = app.model_picker.as_mut() {
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
                if app.variant_picker.is_some() {
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
                if let Some(palette) = app.command_palette.as_mut() {
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
                        let background = spawner.running_background();
                        if background > 0 {
                            spawner.abort_all();
                            notifications_paused = true;
                            app.status = "background notifications paused".into();
                            app.set_persistent_notice(
                                "background jobs cancelled; notifications paused; send a message to resume",
                                NoticeLevel::Warning,
                            );
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
                                app.set_notice("aborting turn…", NoticeLevel::Warning);
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
                            let loop_config = loop_config.clone();
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
                        PromptAction::Edited => app.clear_transient_notice(),
                        PromptAction::Unhandled | PromptAction::Submit => {}
                    },
                }
            }
            Event::Paste(text) if app.command_palette.is_some() => {
                app.command_palette.as_mut().unwrap().insert_query(&text);
            }
            Event::Paste(text) if !app.has_modal() => {
                app.model_key_pending = false;
                app.clear_transient_notice();
                app.input.insert(&text);
            }
            Event::Mouse(mouse) if !app.has_modal() => match mouse.kind {
                kind @ (MouseEventKind::ScrollUp | MouseEventKind::ScrollDown) => {
                    let initial_rows = if kind == MouseEventKind::ScrollUp {
                        -3
                    } else {
                        3
                    };
                    let batch =
                        drain_wheel_batch(initial_rows, MAX_WHEEL_EVENTS_PER_BATCH, || {
                            if crossterm::event::poll(std::time::Duration::ZERO)? {
                                Ok(Some(crossterm::event::read()?))
                            } else {
                                Ok(None)
                            }
                        })?;
                    pending_terminal_event = batch.deferred;
                    app.scroll_wheel(batch.rows);
                }
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
                    ilar::session::ContentBlock::ReasoningSummary {
                        text: "**Reviewing restored state**\n\nDetails remain collapsed.".into(),
                        completed: true,
                    },
                    ilar::session::ContentBlock::ToolCall {
                        id: "read-1".into(),
                        name: "read".into(),
                        input: Default::default(),
                    },
                    ilar::session::ContentBlock::ToolCall {
                        id: "task-1".into(),
                        name: "task".into(),
                        input: serde_json::json!({
                            "description": "Review restored security paths",
                            "subagent_type": "build · secure",
                        }),
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
                tool_use_id: "task-1".into(),
                content: "review complete".into(),
                is_error: false,
                child_session_id: None,
                state: None,
                ts: chrono::Utc::now(),
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::ToolResult {
                id: new_id(),
                tool_use_id: "read-1".into(),
                content: "file contents".into(),
                is_error: false,
                child_session_id: None,
                state: None,
                ts: chrono::Utc::now(),
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::ModelChange {
                id: new_id(),
                model: "openai/gpt-5.6-sol".into(),
                variant: Some("high".into()),
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
            Line_::Thought { text, complete: true }
                if text.contains("Reviewing restored state")
        ));
        assert!(matches!(
            &view.lines[3],
            Line_::Tool { id, name, arguments, state: ToolState::Succeeded, .. }
                if id == "read-1" && name == "read" && arguments.is_empty()
        ));
        assert!(matches!(
            &view.lines[4],
            Line_::Tool {
                id,
                name,
                kind: ToolKind::Agent { name: agent },
                arguments,
                state: ToolState::Succeeded,
                ..
            } if id == "task-1"
                && name == "task"
                && agent == "build · secure"
                && arguments == "Review restored security paths"
        ));
        assert!(matches!(
            view.lines.last(),
            Some(Line_::System(text)) if text.contains("openai/gpt-5.6-sol")
        ));
        let rendered = format!("{:?}", view.lines);
        assert!(!rendered.contains("hidden thought"), "{rendered}");
    }

    #[test]
    fn restored_edit_tools_carry_a_diff() {
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
                    id: "edit-1".into(),
                    name: "edit".into(),
                    input: serde_json::json!({
                        "path": "src/lib.rs",
                        "old_string": "keep\nold",
                        "new_string": "keep\nnew",
                    }),
                }],
                usage: ilar::session::Usage::default(),
                stop_reason: "tool_use".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(session);

        let view = restored_session_view(&store.load(&session_id).unwrap());
        let Some(Line_::Tool { diff, .. }) = view.lines.first() else {
            panic!("expected restored edit tool: {:?}", view.lines);
        };
        assert_eq!(
            diff.iter().map(|line| line.kind).collect::<Vec<_>>(),
            vec![
                diff::DiffKind::Context,
                diff::DiffKind::Removed,
                diff::DiffKind::Added
            ]
        );
    }

    #[test]
    fn tool_diff_rows_truncate_and_expand() {
        let diff: Vec<diff::DiffLine> = (0..12)
            .map(|index| diff::DiffLine {
                kind: diff::DiffKind::Added,
                text: format!("added line {index}"),
            })
            .collect();
        let limited = tool_diff_rows(&diff, 80, 4, 8);
        assert_eq!(limited.len(), 8);
        assert!(rendered_text(&limited.last().unwrap().line).contains("… more"));
        assert!(
            !limited
                .iter()
                .any(|row| rendered_text(&row.line).contains("added line 11"))
        );

        let full = tool_diff_rows(&diff, 80, 4, usize::MAX);
        assert_eq!(full.len(), 12);
        assert!(
            full.iter()
                .any(|row| rendered_text(&row.line).contains("added line 11"))
        );
        assert!(
            !full
                .iter()
                .any(|row| rendered_text(&row.line).contains("… more"))
        );
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
            rendered.contains("task Assess architecture and risks completed."),
            "{rendered}"
        );
        assert!(!rendered.contains("you  Task"), "{rendered}");
        assert!(
            rendered.contains("job  job-1 (\"Run checks\") completed."),
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
            rendered.contains("task Live review completed."),
            "{rendered}"
        );
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

        let status = rendered_text(&app.status_line(120));
        assert!(status.contains("openai/gpt-5.6-sol@high"), "{status}");
        assert!(status.contains("in 300"), "{status}");
        assert!(status.contains("out 50"), "{status}");
        assert!(status.contains("req cache r1500/w20"), "{status}");
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
    fn transcript_uses_neutral_body_text_and_distinct_reasoning_color() {
        let now = std::time::Instant::now();
        let assistant =
            transcript_entry_lines(&Line_::Assistant("plain response".into()), 80, now, now);
        assert_eq!(assistant[0].spans[0].style.fg, Some(theme::ASSISTANT));
        assert_eq!(assistant[0].spans[1].style.fg, Some(theme::PRIMARY));

        let user = transcript_entry_lines(&Line_::User("plain request".into()), 80, now, now);
        assert_eq!(user[0].spans[0].style.fg, Some(theme::USER));
        assert_eq!(user[0].spans[1].style.fg, Some(theme::PRIMARY));

        let thought = transcript_entry_lines(
            &Line_::Thought {
                text: "Inspecting state".into(),
                complete: true,
            },
            80,
            now,
            now,
        );
        assert_eq!(thought[0].spans[0].style.fg, Some(theme::REASONING));
        assert_ne!(theme::REASONING, theme::WAITING);
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
        assert!(matches!(
            picker.handle_key(KeyCode::Enter, false),
            PickerAction::Choose(_)
        ));

        let model = ilar::model::find("openai/gpt-4.1").unwrap();
        let mut picker = ModelPicker::new(vec![model], &model.full_id());
        assert_eq!(
            picker.handle_key(KeyCode::Enter, false),
            PickerAction::Dismiss
        );
    }

    #[test]
    fn command_palette_searches_and_selects_defined_commands() {
        let mut palette = CommandPalette::new();

        assert_eq!(palette.filtered_commands().len(), 4);
        assert_eq!(
            palette.handle_key(KeyCode::Enter, false),
            CommandPaletteAction::Choose(PaletteCommand::Model)
        );

        palette.insert_query("sessio");
        assert_eq!(
            palette.handle_key(KeyCode::Enter, false),
            CommandPaletteAction::Choose(PaletteCommand::Session)
        );

        palette.insert_query("nomatchhere");
        assert!(palette.filtered_commands().is_empty());
        assert_eq!(
            palette.handle_key(KeyCode::Enter, false),
            CommandPaletteAction::Stay
        );
        assert_eq!(
            palette.handle_key(KeyCode::Esc, false),
            CommandPaletteAction::Dismiss
        );

        let mut palette = CommandPalette::new();
        palette.insert_query("model 🚀\n");
        assert_eq!(palette.query, "model 🚀");
        palette.handle_key(KeyCode::Backspace, false);
        assert_eq!(palette.query, "model ");

        let mut palette = CommandPalette::new();
        palette.insert_query("theme");
        assert_eq!(palette.filtered_commands().len(), 1);
        assert_eq!(
            palette.handle_key(KeyCode::Enter, false),
            CommandPaletteAction::Choose(PaletteCommand::Theme)
        );
    }

    #[test]
    fn session_picker_navigates_and_chooses() {
        let now = std::time::SystemTime::now();
        let sessions = vec![
            ilar::session::SessionSummary {
                id: "recent".into(),
                title: Some("latest work".into()),
                modified: now,
            },
            ilar::session::SessionSummary {
                id: "older".into(),
                title: None,
                modified: now - std::time::Duration::from_secs(3_600),
            },
        ];
        let mut picker = SessionPicker::new(sessions);
        assert_eq!(picker.handle_key(KeyCode::Down, false), PickerAction::Stay);
        assert_eq!(
            picker.handle_key(KeyCode::Enter, false),
            PickerAction::Choose("older".into())
        );
        picker.move_selection(1); // wraps back to the first entry
        assert_eq!(
            picker.handle_key(KeyCode::Enter, false),
            PickerAction::Choose("recent".into())
        );
        assert_eq!(
            picker.handle_key(KeyCode::Esc, false),
            PickerAction::Dismiss
        );

        let mut empty = SessionPicker::new(Vec::new());
        assert_eq!(
            empty.handle_key(KeyCode::Enter, false),
            PickerAction::Dismiss
        );
    }

    #[test]
    fn session_age_buckets() {
        let now = std::time::SystemTime::now();
        let at = |seconds: u64| now - std::time::Duration::from_secs(seconds);
        assert_eq!(session_age(at(5), now), "now");
        assert_eq!(session_age(at(90), now), "1m");
        assert_eq!(session_age(at(7_200), now), "2h");
        assert_eq!(session_age(at(200_000), now), "2d");
        // Clock skew (mtime in the future) must not panic.
        assert_eq!(
            session_age(now + std::time::Duration::from_secs(60), now),
            "now"
        );
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
            PaletteCommand::Model,
            ilar::model::catalog().iter().collect(),
        );
        assert!(app.command_palette.is_none());
        assert!(app.model_picker.is_some());
        assert_eq!(app.input.text(), "draft prompt");

        app.model_picker = None;
        app.current_model = "openai/gpt-5.2".into();
        app.current_variant = Some("high".into());
        app.command_palette = Some(CommandPalette::new());
        activate_palette_command(
            &mut app,
            PaletteCommand::Reasoning,
            ilar::model::catalog().iter().collect(),
        );
        assert!(app.command_palette.is_none());
        assert!(app.variant_picker.is_some());

        app.variant_picker = None;
        app.command_palette = Some(CommandPalette::new());
        activate_palette_command(
            &mut app,
            PaletteCommand::Theme,
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
    fn reasoning_variant_picker_includes_default_and_current_level() {
        let model = ilar::model::find("openai/gpt-5.2").unwrap();
        let mut picker = VariantPicker::new(model, Some("high"));

        assert_eq!(picker.selected_index(), 4);
        assert_eq!(
            picker.handle_key(KeyCode::Home, false),
            VariantPickerAction::Stay
        );
        assert_eq!(
            picker.handle_key(KeyCode::Enter, false),
            VariantPickerAction::Choose(None)
        );
        assert_eq!(
            picker.handle_key(KeyCode::Esc, false),
            VariantPickerAction::Dismiss
        );
    }

    #[test]
    fn command_palette_renders_a_selectable_command_on_narrow_terminals() {
        let backend = ratatui::backend::TestBackend::new(30, 6);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.command_palette = Some(CommandPalette::new());

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
    fn styled_wrap_prefers_words_and_never_adds_blank_rows() {
        let code = markdown::render("```\n    hello world\n```", usize::MAX).remove(0);
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

        let inline = markdown::render("`│ alpha beta`", usize::MAX).remove(0);
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

    #[test]
    fn markdown_tables_use_the_transcript_content_width() {
        let now = std::time::Instant::now();
        let rows = transcript_entry_lines(
            &Line_::Assistant(
                "| Phase | Estimate |\n| --- | ---: |\n| Signed-device testing | 1–2 weeks |"
                    .into(),
            ),
            26,
            now,
            now,
        );
        let rendered = rows.iter().map(rendered_text).collect::<Vec<_>>();

        assert!(rendered.iter().all(|line| line.width() <= 26));
        assert!(rendered.iter().any(|line| line.contains("Phase:")));
        assert!(!rendered.iter().any(|line| line.contains("---")));
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
                text: "Answering".into(),
                complete: true,
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
    fn restored_agent_loads_its_child_timeline() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path().join("sessions"));
        let parent_id = new_id();
        let child_id = new_id();
        let mut child = store
            .create(SessionMeta {
                session_id: child_id.clone(),
                parent_id: Some(parent_id.clone()),
                agent: "explore".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
            })
            .unwrap();
        child
            .append(ilar::session::SessionEvent::SubagentInvocation {
                id: new_id(),
                parent_tool_call_id: "task-restore".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        child
            .append(ilar::session::SessionEvent::UserMessage {
                id: new_id(),
                text: "Inspect rendering".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        child
            .append(ilar::session::SessionEvent::AssistantMessage {
                id: new_id(),
                model: "zai/glm-4.7".into(),
                content: vec![ilar::session::ContentBlock::Text {
                    text: "Nested restored answer".into(),
                }],
                usage: Default::default(),
                stop_reason: "end_turn".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        child
            .append(ilar::session::SessionEvent::SubagentInvocation {
                id: new_id(),
                parent_tool_call_id: "later-task".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        child
            .append(ilar::session::SessionEvent::UserMessage {
                id: new_id(),
                text: "Later request".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        child
            .append(ilar::session::SessionEvent::AssistantMessage {
                id: new_id(),
                model: "zai/glm-4.7".into(),
                content: vec![ilar::session::ContentBlock::Text {
                    text: "Later answer".into(),
                }],
                usage: Default::default(),
                stop_reason: "end_turn".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(child);

        let mut parent = store
            .create(SessionMeta {
                session_id: parent_id.clone(),
                parent_id: None,
                agent: "build".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
            })
            .unwrap();
        parent
            .append(ilar::session::SessionEvent::AssistantMessage {
                id: new_id(),
                model: "zai/glm-4.7".into(),
                content: vec![ilar::session::ContentBlock::ToolCall {
                    id: "task-restore".into(),
                    name: "task".into(),
                    input: serde_json::json!({
                        "description": "Inspect rendering",
                        "subagent_type": "explore"
                    }),
                }],
                usage: Default::default(),
                stop_reason: "tool_use".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        parent
            .append(ilar::session::SessionEvent::ToolResult {
                id: new_id(),
                tool_use_id: "task-restore".into(),
                content: "Nested restored answer".into(),
                is_error: false,
                child_session_id: Some(child_id),
                state: None,
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(parent);

        let restored = restored_session_view_with_store(&store.load(&parent_id).unwrap(), &store);
        let child_lines = restored.lines.iter().find_map(|line| match line {
            Line_::Tool { child_lines, .. } => Some(child_lines),
            _ => None,
        });

        assert!(child_lines.is_some_and(|lines| {
            lines.iter().any(
                |line| matches!(line, Line_::Assistant(text) if text == "Nested restored answer"),
            ) && !lines
                .iter()
                .any(|line| matches!(line, Line_::Assistant(text) if text == "Later answer"))
        }));
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
        let narrow_agent = rendered_text(&tool_line(
            "task",
            &ToolKind::Agent {
                name: "repository-reviewer".into(),
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
    fn tool_rows_never_exceed_their_width() {
        for width in 0..=100 {
            let line = tool_line(
                "extremely-long-tool-name",
                &ToolKind::Tool,
                "👩‍💻 /very/long/path/to/a/file with arguments",
                ToolState::Succeeded,
                width,
                std::time::Duration::ZERO,
                ToolProgress::None,
                std::time::Instant::now(),
            );
            let rendered = rendered_text(&line);
            assert!(
                UnicodeWidthStr::width(rendered.as_str()) <= width as usize,
                "width {width}: {rendered:?}"
            );
            let now = std::time::Instant::now();
            let progress = tool_line(
                "write",
                &ToolKind::Tool,
                "👩‍💻 /very/long/path/to/a/file",
                ToolState::Running,
                width,
                std::time::Duration::ZERO,
                ToolProgress::Receiving {
                    received_bytes: u64::MAX,
                    last_data: now - std::time::Duration::from_secs(30),
                },
                now,
            );
            let rendered = rendered_text(&progress);
            assert!(
                UnicodeWidthStr::width(rendered.as_str()) <= width as usize,
                "progress width {width}: {rendered:?}"
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
