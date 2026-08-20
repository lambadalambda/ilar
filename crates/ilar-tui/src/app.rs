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
    Truncation, abbreviated_path, bounded_detail, context_meter, context_usage, format_bytes,
    format_cost, format_tokens_compact, safe_lines, safe_text, truncate_display, wrap_styled_line,
};
use crate::transcript::{
    Line_, ToolKind, ToolProgress, ToolState, TranscriptHitTarget, TranscriptRenderCache,
    append_thought_tail, apply_subagent_activity, toggle_tool_expansion, transcript_markdown,
};
use crate::{
    Activity, ERROR, MAX_GOAL_ROUNDS, MUTED, NoticeLevel, history, slash_candidates, theme,
};

pub(crate) struct App {
    pub(crate) lines: Vec<Line_>,
    pub(crate) input: InputBuffer,
    pub(crate) history: history::PromptHistory,
    pub(crate) busy: bool,
    pub(crate) status: String,
    notice: Option<StatusNotice>,
    activity: Activity,
    activity_started: std::time::Instant,
    pub(crate) current_model: String,
    pub(crate) current_variant: Option<String>,
    pub(crate) session_id: String,
    cwd: std::path::PathBuf,
    pub(crate) context_used: u64,
    pub(crate) context_limit: Option<u64>,
    pub(crate) context_estimated: bool,
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
    pub(crate) last_prompt: Option<String>,
    pub(crate) retry_available: bool,
    /// Messages submitted during an active turn, auto-sent in order when
    /// the turn completes.
    pub(crate) queued_messages: Vec<String>,
    /// Steers handed to a running turn but not yet delivered. Steering
    /// is fire-and-forget, so an aborted turn drops its receiver and
    /// would lose them silently; these get moved back to the queue.
    pub(crate) pending_steers: Vec<String>,
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
    search_matches: Vec<usize>,
    search_current: usize,
    /// (scroll_top, follow_tail) before the search opened; Esc restores.
    search_saved: Option<(usize, bool)>,
    search_computed_revision: Option<u64>,
    scroll_top: usize,
    content_rows: usize,
    viewport_rows: usize,
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
    /// User-invoked commands. Whole values rather than name/description
    /// pairs, so honouring `agent`/`model`/`variant` later is additive.
    pub(crate) commands: Vec<ilar::command::Command>,
    pub(crate) theme: theme::ThemeId,
    pub(crate) theme_picker: Option<ThemePicker>,
    /// Whether the terminal speaks the kitty keyboard protocol. Without
    /// it Ctrl-M is indistinguishable from Enter, so the help overlay
    /// must not advertise it.
    pub(crate) keyboard_enhanced: bool,
    pub(crate) model_key_pending: bool,
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
            pending_steers: Vec::new(),
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
            commands: Vec::new(),
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

    /// Everything `/` can invoke, in the precedence the dispatcher
    /// uses: the built-in, then commands, then skills a command has not
    /// shadowed. Completion, the picker and near-match suggestions all
    /// read this, so they cannot disagree about what exists.
    pub(crate) fn slash_inventory(&self) -> Vec<(String, String)> {
        let mut entries: Vec<(String, String)> = self
            .commands
            .iter()
            .map(|command| (command.name.clone(), command.description.clone()))
            .collect();
        entries.extend(
            self.skills
                .iter()
                .filter(|(skill, _)| !self.commands.iter().any(|c| &c.name == skill))
                .cloned(),
        );
        entries
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
            // Shown when the loop delivers it, not when it was typed —
            // the transcript reflects what the model actually saw.
            LoopEvent::Steered { text } => {
                if let Some(index) = self.pending_steers.iter().position(|held| held == text) {
                    self.pending_steers.remove(index);
                }
                self.push_transcript_line(Line_::User(text.clone()));
                self.follow_tail = true;
            }
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

    fn max_scroll(&self) -> usize {
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
    fn transcript_lines(&self, width: u16, now: std::time::Instant) -> Vec<Line<'static>> {
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
        if !self.pending_steers.is_empty() {
            input_title = format!("{}· {} steering ", input_title, self.pending_steers.len());
        }
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
        let candidates = slash_candidates(self.input.text(), &self.slash_inventory());
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
            app.skill_picker = Some(SkillPicker::new(app.slash_inventory()));
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct StatusNotice {
    text: String,
    level: NoticeLevel,
    persistent: bool,
}
const ASSISTANT: Color = theme::ASSISTANT;
const TOOL_ACTIVE: Color = theme::RUNNING;
const CONTENT_HORIZONTAL_PADDING: u16 = 2;
/// Show "no data Ns" in the status line once the stream has been silent
/// this long during thinking/responding.
const STREAM_STALL_AFTER: std::time::Duration = std::time::Duration::from_secs(3);

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

#[cfg(test)]
mod tests {
    use super::*;

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
    use crate::modals::{CommandPaletteAction, PALETTE_COMMANDS, is_command_palette_shortcut};
    use crate::selection::SelectionPoint;
    use crate::session_view::restored_session_view;
    use crate::text::tests::rendered_text;
    use crate::transcript::{reasoning_summary_title, tool_line, transcript_entry_lines};
    use crate::{begin_retry, drain_wheel_batch, slash_candidates};
    use crossterm::event::{Event, KeyEvent, KeyModifiers, MouseEventKind};
    use ilar::session::{SessionMeta, new_id};

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

    #[test]
    fn transcript_selection_highlights_cells_and_scrolling_clears_it() {
        let area = Rect::new(0, 0, 4, 2);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
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

    fn command(name: &str, description: &str, template: &str) -> ilar::command::Command {
        ilar::command::Command {
            name: name.into(),
            description: description.into(),
            template: template.into(),
            agent: None,
            model: None,
            variant: None,
        }
    }

    /// A command's body is the prompt. A skill's invocation is a request
    /// for the model to go and load one — the distinction is the whole
    /// point of having both, so exercise the dispatcher rather than
    /// re-implementing it.
    #[test]
    fn a_command_expands_to_its_body_while_a_skill_asks_the_model_to_load_one() {
        let mut app = App::new();
        app.commands = vec![command(
            "greptile",
            "Address Greptile PR comments",
            "Address Greptile feedback.\n\nCommand arguments: $ARGUMENTS",
        )];
        app.skills = vec![("repo-issues".into(), "Manage issues".into())];

        match crate::resolve_slash(&app, "greptile", "PR 41") {
            crate::SlashResolution::Prompt(text) => {
                assert!(text.contains("Command arguments: PR 41"), "{text}");
                assert!(!text.contains("skill` tool"), "{text}");
            }
            other => panic!("expected a prompt, got {other:?}"),
        }
        assert!(matches!(
            crate::resolve_slash(&app, "repo-issues", ""),
            crate::SlashResolution::Skill(prompt) if prompt.contains("`skill` tool")
        ));
    }

    /// A command and a skill can share a name; the command wins, and
    /// every surface agrees because they read one inventory.
    #[test]
    fn a_command_shadows_a_skill_of_the_same_name_consistently() {
        let mut app = App::new();
        app.commands = vec![command("review", "Command review", "Command body")];
        app.skills = vec![
            ("review".into(), "Skill review".into()),
            ("other".into(), "Untouched".into()),
        ];

        assert!(matches!(
            crate::resolve_slash(&app, "review", ""),
            crate::SlashResolution::Prompt(text) if text == "Command body"
        ));
        let inventory = app.slash_inventory();
        assert_eq!(
            inventory
                .iter()
                .filter(|(name, _)| name == "review")
                .count(),
            1,
            "the shadowed skill must not also be listed: {inventory:?}"
        );
        assert_eq!(inventory[0].1, "Command review");
        assert!(inventory.iter().any(|(name, _)| name == "other"));
    }

    /// A body of only placeholders with no arguments expands to nothing,
    /// and an empty prompt is rejected by the provider.
    #[test]
    fn a_command_that_expands_to_nothing_is_refused() {
        let mut app = App::new();
        app.commands = vec![command("echo", "Echo", "$ARGUMENTS")];
        assert_eq!(
            crate::resolve_slash(&app, "echo", ""),
            crate::SlashResolution::Empty
        );
        assert!(matches!(
            crate::resolve_slash(&app, "echo", "something"),
            crate::SlashResolution::Prompt(text) if text == "something"
        ));
    }

    /// The built-in `goal` outranks everything and must not be offered
    /// twice when a command or skill shares its name.
    #[test]
    fn the_goal_builtin_is_listed_once_and_suggested_on_a_typo() {
        let mut app = App::new();
        app.commands = vec![command("goal", "A shadowing command", "body")];
        let candidates = slash_candidates("/goal", &app.slash_inventory());
        assert_eq!(
            candidates.iter().filter(|(name, _)| name == "goal").count(),
            1,
            "{candidates:?}"
        );
        assert!(matches!(
            crate::resolve_slash(&app, "gaol", ""),
            crate::SlashResolution::Unknown(matches) if matches.iter().any(|m| m == "goal")
        ));
    }
}
