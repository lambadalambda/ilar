//! `App`: everything on screen, and the pass that draws it.
//!
//! Holds the transcript, input, search and modal state, folds loop
//! events into it, and renders one frame. The event loop lives in
//! main.rs and drives this; the only I/O is the clipboard, and session
//! replay when a session is restored.

use anyhow::{Context, Result};
use crossterm::event::KeyCode;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, Padding, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState,
};
use unicode_width::UnicodeWidthStr;

use ilar::agent::{LoopEvent, TurnOutcome};
use ilar::session::SessionStore;

use crate::diff;
use crate::input::{InputBuffer, input_accepts_keys};
use crate::modals::{
    CommandPalette, Modal, ModelPicker, PaletteAction, PaletteCommand, PendingAction, PendingItem,
    PendingManager, SessionPicker, SkillPicker, ThemePicker, ThemePickerAction, VariantPicker,
    palette_items, render_command_palette, render_help, render_model_picker,
    render_pending_manager, render_session_picker, render_skill_picker, render_theme_picker,
    render_variant_picker,
};
use crate::selection::{
    RenderedRow, TranscriptSelection, highlight_transcript_selection, selected_rows_unchanged,
    selected_transcript_text, selection_point, transcript_cells,
};
use crate::session_view::{
    accrue_usage, restored_session_view_with_store, task_notification_display,
    tool_notification_display,
};
use crate::sidebar::{
    content_areas, render_todo_sidebar_snapshot, todo_render_snapshot, todo_sidebar_snapshot,
    todo_summary,
};
use crate::text::{
    Truncation, abbreviated_path, bounded_detail, context_meter, context_usage, format_cost,
    format_tokens_compact, safe_lines, safe_text, truncate_display, wrap_styled_line,
};
use crate::transcript::{
    Line_, ToolKind, ToolProgress, ToolState, TranscriptHitTarget, TranscriptRenderCache,
    append_thought_tail, apply_subagent_activity, toggle_tool_expansion, transcript_markdown,
};
use crate::{
    ASSISTANT, Activity, CONTENT_HORIZONTAL_PADDING, ERROR, MAX_GOAL_ROUNDS, MUTED, NoticeLevel,
    StatusNotice, TOOL_ACTIVE, activity_line, history, slash_candidates, stream_liveness, theme,
    windowed_rate,
};

pub(crate) struct App {
    pub(crate) lines: Vec<Line_>,
    pub(crate) input: InputBuffer,
    pub(crate) history: history::PromptHistory,
    pub(crate) busy: bool,
    pub(crate) status: String,
    pub(crate) notice: Option<StatusNotice>,
    activity: Activity,
    pub(crate) activity_started: std::time::Instant,
    pub(crate) current_model: String,
    pub(crate) current_variant: Option<String>,
    pub(crate) session_id: String,
    cwd: std::path::PathBuf,
    pub(crate) context_used: u64,
    pub(crate) context_limit: Option<u64>,
    pub(crate) context_estimated: bool,
    pub(crate) latest_usage: Option<ilar::session::Usage>,
    pub(crate) session_usage: ilar::session::Usage,
    pub(crate) session_cost: Option<f64>,
    /// Bytes of streamed text/thinking received this turn, plus the last
    /// arrival instant — the status line's stream-liveness indicator.
    stream_received: u64,
    pub(crate) stream_last_data: Option<std::time::Instant>,
    /// Bytes already attributed to completed steps; the live output
    /// estimate uses only the current step's bytes.
    stream_step_base: u64,
    /// Windowed transfer rate: anchor of the current >=1s window and the
    /// last completed window's bytes/sec.
    stream_rate_anchor: Option<(std::time::Instant, u64)>,
    stream_rate: Option<f64>,
    /// Last submitted prompt, offered for Ctrl-R retry after a turn error.
    pub(crate) last_prompt: Option<String>,
    pub(crate) retry_available: bool,
    /// Messages submitted during an active turn, auto-sent in order when
    /// the turn completes.
    pub(crate) queued_messages: Vec<String>,
    /// Active goal: (description, completed rounds). Turns auto-continue
    /// until the model emits GOAL_ACHIEVED or the round cap trips.
    pub(crate) goal: Option<(String, u32)>,
    /// Selection inside the inline slash-completion popup.
    pub(crate) slash_selected: usize,
    pub(crate) pending_manager: Option<PendingManager>,
    /// Snapshot of spawner.running_background() for rendering.
    pub(crate) background_running: usize,
    /// Snapshot of the service manager's running count for rendering.
    pub(crate) services_running: usize,
    /// (name, running, detail) rows for the sidebar.
    pub(crate) services_view: Vec<(String, bool, String)>,
    /// Palette-requested compaction, applied to the next turn's config.
    pub(crate) compact_requested: bool,
    pub(crate) search_active: bool,
    pub(crate) search_query: String,
    pub(crate) search_matches: Vec<usize>,
    search_current: usize,
    /// (scroll_top, follow_tail) before the search opened; Esc restores.
    search_saved: Option<(usize, bool)>,
    search_computed_revision: Option<u64>,
    pub(crate) scroll_top: usize,
    pub(crate) content_rows: usize,
    pub(crate) viewport_rows: usize,
    pub(crate) follow_tail: bool,
    pub(crate) command_palette: Option<CommandPalette>,
    pub(crate) model_picker: Option<ModelPicker>,
    pub(crate) variant_picker: Option<VariantPicker>,
    pub(crate) session_picker: Option<SessionPicker>,
    /// Set by the palette; run_app opens the picker (it owns the store).
    pub(crate) session_picker_requested: bool,
    pub(crate) help_visible: bool,
    pub(crate) help_scroll: usize,
    pub(crate) skill_picker: Option<SkillPicker>,
    /// (name, description) pairs for slash invocation and the picker.
    pub(crate) skills: Vec<(String, String)>,
    pub(crate) theme: theme::ThemeId,
    pub(crate) theme_picker: Option<ThemePicker>,
    /// Whether the terminal speaks the kitty keyboard protocol. Without
    /// it Ctrl-M is indistinguishable from Enter, so the help overlay
    /// must not advertise it.
    pub(crate) keyboard_enhanced: bool,
    pub(crate) model_key_pending: bool,
    pub(crate) transcript_text_area: Rect,
    pub(crate) transcript_cache: TranscriptRenderCache,
    pub(crate) transcript_hit_targets: Vec<Option<TranscriptHitTarget>>,
    pub(crate) transcript_cells: Vec<RenderedRow>,
    pub(crate) transcript_selection: Option<TranscriptSelection>,
    pub(crate) selecting_transcript: bool,
    transcript_dragged: bool,
    clipboard: Option<arboard::Clipboard>,
    next_tool_group: u64,
    next_thought: u64,
    pub(crate) expanded_tool_groups: std::collections::HashSet<String>,
    pub(crate) transcript_revision: u64,
    pub(crate) pending_subagent_activity:
        std::collections::VecDeque<ilar::subagent::SubagentActivity>,
    pub(crate) todos: std::sync::Arc<std::sync::Mutex<ilar::todo::TodoList>>,
}

impl App {
    pub(crate) fn new() -> Self {
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

    pub(crate) fn open_command_palette(&mut self) {
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
    pub(crate) fn active_modal(&self) -> Option<Modal> {
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

    pub(crate) fn has_modal(&self) -> bool {
        self.active_modal().is_some()
    }

    /// Route a wheel batch to whatever overlay is in front. Returns
    /// false when nothing consumed it, so the transcript can scroll.
    pub(crate) fn scroll_active_modal(&mut self, rows: isize) -> bool {
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

    pub(crate) fn configure_runtime(
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

    pub(crate) fn restore_session(
        &mut self,
        session: &ilar::session::SessionReader,
        store: &SessionStore,
    ) {
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

    pub(crate) fn set_activity(&mut self, activity: Activity) {
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

    pub(crate) fn push_transcript_line(&mut self, line: Line_) {
        self.lines.push(line);
        self.transcript_revision = self.transcript_revision.wrapping_add(1);
    }

    pub(crate) fn push_notification(&mut self, description: &str, text: &str) {
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

    pub(crate) fn push_loop_event(&mut self, event: &LoopEvent) {
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

    pub(crate) fn push_subagent_activity(&mut self, activity: &ilar::subagent::SubagentActivity) {
        self.transcript_revision = self.transcript_revision.wrapping_add(1);
        if !apply_subagent_activity(&mut self.lines, &self.session_id, activity)
            && self.pending_subagent_activity.len() < 256
        {
            self.pending_subagent_activity.push_back(activity.clone());
        }
        self.retry_subagent_activity();
    }

    pub(crate) fn retry_subagent_activity(&mut self) {
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

    pub(crate) fn finish_turn(&mut self, result: anyhow::Result<TurnOutcome>) {
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

    pub(crate) fn pending_items(&self) -> Vec<PendingItem> {
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

    pub(crate) fn pending_manager_key(&mut self, code: KeyCode, control: bool) -> PendingAction {
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

    pub(crate) fn open_search(&mut self) {
        self.search_active = true;
        if self.search_saved.is_none() {
            self.search_saved = Some((self.scroll_top, self.follow_tail));
        }
        self.search_refresh();
    }

    /// Recompute matches against the cached rows and jump to the first.
    pub(crate) fn search_refresh(&mut self) {
        self.search_matches = self.transcript_cache.matching_rows(&self.search_query);
        self.search_computed_revision = Some(self.transcript_revision);
        self.search_current = 0;
        if !self.search_matches.is_empty() {
            self.search_scroll_to_current();
        }
    }

    pub(crate) fn search_jump(&mut self, delta: isize) {
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

    pub(crate) fn close_search(&mut self, restore_scroll: bool) {
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

    pub(crate) fn max_scroll(&self) -> usize {
        self.content_rows.saturating_sub(self.viewport_rows)
    }

    pub(crate) fn page_size(&self) -> usize {
        self.viewport_rows.saturating_sub(2).max(1)
    }

    pub(crate) fn scroll_up(&mut self, rows: usize) {
        self.clear_transcript_selection();
        self.follow_tail = false;
        self.scroll_top = self.scroll_top.saturating_sub(rows);
    }

    pub(crate) fn scroll_down(&mut self, rows: usize) {
        self.clear_transcript_selection();
        let max_scroll = self.max_scroll();
        self.scroll_top = self.scroll_top.saturating_add(rows).min(max_scroll);
        self.follow_tail = self.scroll_top == max_scroll;
    }

    pub(crate) fn scroll_wheel(&mut self, rows: isize) {
        self.clear_transcript_selection();
        if rows < 0 {
            self.scroll_up(rows.unsigned_abs());
        } else if rows > 0 {
            self.scroll_down(rows as usize);
        }
    }

    pub(crate) fn scroll_to_top(&mut self) {
        self.clear_transcript_selection();
        self.scroll_top = 0;
        self.follow_tail = self.max_scroll() == 0;
    }

    pub(crate) fn scroll_to_tail(&mut self) {
        self.clear_transcript_selection();
        self.scroll_top = self.max_scroll();
        self.follow_tail = true;
    }

    pub(crate) fn update_scroll_metrics(&mut self, content_rows: usize, viewport_rows: usize) {
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

    pub(crate) fn begin_transcript_selection(&mut self, column: u16, row: u16) {
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

    pub(crate) fn update_transcript_selection(&mut self, column: u16, row: u16) {
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

    pub(crate) fn drag_transcript_selection(&mut self, column: u16, row: u16) {
        self.transcript_dragged = true;
        self.update_transcript_selection(column, row);
    }

    pub(crate) fn finish_transcript_selection(&mut self, column: u16, row: u16) -> Option<String> {
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

    pub(crate) fn toggle_transcript_target(&mut self, target: TranscriptHitTarget) {
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

    pub(crate) fn copy_to_clipboard(&mut self, text: &str) -> Result<()> {
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
    pub(crate) fn transcript_lines(
        &self,
        width: u16,
        now: std::time::Instant,
    ) -> Vec<Line<'static>> {
        use crate::transcript::{transcript_entries, transcript_entry_rows};

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

    pub(crate) fn model_status_label(&self, include_provider: bool, width: usize) -> String {
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

    pub(crate) fn set_notice(&mut self, text: impl Into<String>, level: NoticeLevel) {
        self.set_notice_with_lifetime(text, level, level == NoticeLevel::Error);
    }

    pub(crate) fn set_persistent_notice(&mut self, text: impl Into<String>, level: NoticeLevel) {
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

    pub(crate) fn clear_notice(&mut self) {
        self.notice = None;
    }

    pub(crate) fn clear_transient_notice(&mut self) {
        if self
            .notice
            .as_ref()
            .is_some_and(|notice| !notice.persistent)
        {
            self.notice = None;
        }
    }

    pub(crate) fn operational_notice(&self) -> Option<(&str, Color)> {
        self.notice.as_ref().map(|notice| {
            let color = match notice.level {
                NoticeLevel::Info => theme::PRIMARY,
                NoticeLevel::Warning => theme::WAITING,
                NoticeLevel::Error => theme::ERROR,
            };
            (notice.text.as_str(), color)
        })
    }

    pub(crate) fn status_line(&self, width: u16) -> Line<'static> {
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

    pub(crate) fn render(&mut self, frame: &mut Frame) {
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

pub(crate) fn apply_theme_picker_action(
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

pub(crate) fn activate_palette_command(
    app: &mut App,
    action: PaletteAction,
    model_choices: Vec<&'static ilar::model::ModelInfo>,
) {
    app.command_palette = None;
    let PaletteAction::Command(command) = action;
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
        PaletteCommand::Usage => {
            let total = app.session_usage;
            let cost = match app.session_cost {
                Some(cost) => format_cost(cost),
                None if ilar::model::plan_billed(&app.current_model) => {
                    "subscription plan (no per-token cost)".into()
                }
                None => "unknown (model without pricing)".into(),
            };
            app.push_transcript_line(Line_::System(format!(
                "session usage\ninput {} · output {} · cache read {} · cache write {}\nestimated cost {cost} (list prices, {})",
                total.input_tokens,
                total.output_tokens,
                total.cache_read_input_tokens,
                total.cache_creation_input_tokens,
                ilar::model::CATALOG_UPDATED,
            )));
            app.follow_tail = true;
        }
        PaletteCommand::Skills => {
            app.skill_picker = Some(SkillPicker::new(app.skills.clone()));
        }
        PaletteCommand::Compact => {
            app.compact_requested = true;
            app.set_notice(
                "compaction will run before your next message",
                NoticeLevel::Info,
            );
        }
        PaletteCommand::Export => {
            let prefix: String = app.session_id.chars().take(8).collect();
            let path = app.cwd.join(format!("ilar-transcript-{prefix}.md"));
            let markdown = transcript_markdown(&app.session_id, &app.lines);
            match std::fs::write(&path, markdown) {
                Ok(()) => {
                    let message = format!("transcript exported to {}", path.display());
                    app.set_notice(&message, NoticeLevel::Info);
                    app.push_transcript_line(Line_::System(message));
                }
                Err(error) => {
                    app.set_notice(format!("export failed: {error}"), NoticeLevel::Error);
                }
            }
        }
        PaletteCommand::Pending => {
            app.pending_manager = Some(PendingManager::default());
        }
        PaletteCommand::Help => {
            app.help_visible = true;
            app.help_scroll = 0;
        }
    }
}
