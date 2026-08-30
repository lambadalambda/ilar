//! `App`: everything on screen, as state.
//!
//! Holds the transcript, input, search and modal state and folds loop
//! events into it; view.rs turns it into a frame. The event loop lives
//! in main.rs and drives this; the only I/O is the clipboard, and
//! session replay when a session is restored.

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::Color;

use ilar::agent::{LoopEvent, TurnOutcome};
use ilar::session::SessionStore;

use crate::input::{InputBuffer, slash_candidates};
use crate::modals::{
    AsideModal, CommandPalette, LinkPicker, Modal, ModelPicker, PaletteCommand, PendingAction,
    PendingItem, PendingManager, SessionPicker, SessionSearch, SkillPicker, ThemePicker,
    ThemePickerAction, TurnPicker, VariantPicker, palette_items,
};
use crate::questions::QuestionModal;
use crate::selection::{
    RenderedRow, TranscriptSelection, selected_transcript_text, selection_point,
};
use crate::session_view::{
    Liveness, accrue_usage, restored_session_view_with_store, task_notification_display,
    tool_notification_display,
};
use crate::sidebar::{AgentRow, AgentTarget};
use crate::text::{cache_share, format_cost, safe_text};
use crate::transcript::{
    Line_, ToolState, TranscriptHitTarget, TranscriptRenderCache, append_text_delta,
    append_thought_delta, apply_child_loop_event, apply_subagent_activity, complete_open_thought,
    complete_tool_execution,
    complete_tool_input, configure_subagent_row, finish_tool_row, note_tool_input_progress,
    prune_incomplete_thoughts, push_tool_row, set_tool_arguments, set_tool_tail,
    start_tool_execution, toggle_note_expansion, toggle_tool_expansion, tool_group_index,
    transcript_markdown,
};
use crate::{Activity, MAX_GOAL_ROUNDS, NoticeLevel, history, theme};

/// A command invocation that runs as a background subagent — the
/// `subtask`/`agent` half of command frontmatter.
#[derive(Debug, PartialEq)]
pub(crate) struct SubtaskRequest {
    /// Shown in the task listing; the `/name` that started it.
    pub(crate) description: String,
    pub(crate) prompt: String,
    pub(crate) agent: String,
    pub(crate) model: Option<String>,
    pub(crate) variant: Option<String>,
}

/// A child session's transcript taken over the screen: seeded by store
/// replay, then followed live off the `SubagentActivity` feed filtered
/// by session id. A view, not a transfer — the root transcript keeps
/// flowing untouched underneath, and everything stateful (watchdog,
/// notices, deliveries, steers) stays with the root.
pub(crate) struct FocusView {
    pub(crate) session_id: String,
    /// "{agent} · {description}", the view's title.
    pub(crate) title: String,
    pub(crate) lines: Vec<Line_>,
    /// Tool-group counter for the live fold, exactly as a nested
    /// timeline keeps one.
    pub(crate) group: u64,
    /// Cleared by the focused session's own TurnDone; only changes the
    /// footer — a finished agent says so in place, the view stays.
    pub(crate) running: bool,
    pub(crate) scroll_top: usize,
    pub(crate) follow_tail: bool,
    /// Set by the render, like the root transcript's scroll metrics.
    pub(crate) content_rows: usize,
    pub(crate) viewport_rows: usize,
    /// Its own render cache: same machinery as the main transcript,
    /// separate lines. Tool groups stay collapsed — expansion clicks
    /// belong to the root transcript.
    pub(crate) cache: TranscriptRenderCache,
    pub(crate) revision: u64,
    pub(crate) opened: std::time::Instant,
}

impl FocusView {
    pub(crate) fn new(
        session_id: String,
        title: String,
        lines: Vec<Line_>,
        running: bool,
    ) -> Self {
        Self {
            session_id,
            title,
            lines,
            group: 0,
            running,
            scroll_top: 0,
            follow_tail: true,
            content_rows: 0,
            viewport_rows: 0,
            cache: TranscriptRenderCache::default(),
            revision: 0,
            opened: std::time::Instant::now(),
        }
    }

    pub(crate) fn max_scroll(&self) -> usize {
        self.content_rows.saturating_sub(self.viewport_rows)
    }

    pub(crate) fn page_size(&self) -> usize {
        self.viewport_rows.saturating_sub(2).max(1)
    }

    pub(crate) fn scroll_by(&mut self, rows: isize) {
        if rows < 0 {
            self.scroll_top = self.scroll_top.saturating_sub(rows.unsigned_abs());
            self.follow_tail = false;
        } else if rows > 0 {
            let max_scroll = self.max_scroll();
            self.scroll_top = self.scroll_top.saturating_add(rows as usize).min(max_scroll);
            self.follow_tail = self.scroll_top == max_scroll;
        }
    }

    pub(crate) fn scroll_to_top(&mut self) {
        self.scroll_top = 0;
        self.follow_tail = self.max_scroll() == 0;
    }

    pub(crate) fn scroll_to_tail(&mut self) {
        self.scroll_top = self.max_scroll();
        self.follow_tail = true;
    }

    /// A change to the lines: full-rebuild mark, correctness over
    /// narrowness — a focused transcript is one child's, not the
    /// root's whole history.
    fn touch(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.cache.mark_dirty_from(0, self.revision);
    }
}

/// A prompt put aside by Ctrl-S, with whatever was attached to it. The
/// text and its images travel as one unit: stashing only the text would
/// hand the images to the *next* message and give the popped prompt
/// back bare.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct StashedPrompt {
    pub(crate) text: String,
    pub(crate) images: Vec<ilar::session::ImageContent>,
}

/// Whether a queued or steered text is a task/tool notification
/// envelope — a result that left the notification machinery and became
/// an ordinary message. The tag prefix is the cheap screen; the display
/// formatters do the actual parsing where a headline is needed.
fn is_notification_envelope(text: &str) -> bool {
    text.starts_with("<task-notification>") || text.starts_with("<tool-notification>")
}

/// The collapsed headline a queued task/tool result wears in the
/// pending manager — the same first line its transcript row would
/// show. `None` for an ordinary message.
fn queued_result_headline(message: &ilar::agent::Steer) -> Option<String> {
    task_notification_display(&message.text)
        .or_else(|| tool_notification_display(&message.text))
        .map(|display| display.lines().next().unwrap_or_default().to_string())
}

/// The words of every waiting message, for the tests that care about
/// which message is where rather than what is attached to it.
#[cfg(test)]
pub(crate) fn waiting_texts(messages: &[ilar::agent::Steer]) -> Vec<&str> {
    messages
        .iter()
        .map(|message| message.text.as_str())
        .collect()
}

pub(crate) struct App {
    /// Private on purpose: every mutation has to be paired with a
    /// `touch_transcript` so the render cache learns which rows moved,
    /// and that pairing is only checkable where both live. Other modules
    /// read through [`App::lines`]; nothing outside this one writes.
    lines: Vec<Line_>,
    pub(crate) input: InputBuffer,
    pub(crate) history: history::PromptHistory,
    pub(crate) busy: bool,
    pub(crate) status: String,
    notice: Option<StatusNotice>,
    pub(crate) activity: Activity,
    pub(crate) activity_started: std::time::Instant,
    pub(crate) current_model: String,
    pub(crate) current_variant: Option<String>,
    pub(crate) session_id: String,
    pub(crate) cwd: std::path::PathBuf,
    pub(crate) context_used: u64,
    pub(crate) context_limit: Option<u64>,
    pub(crate) context_estimated: bool,
    pub(crate) latest_usage: Option<ilar::session::Usage>,
    pub(crate) session_usage: ilar::session::Usage,
    pub(crate) session_cost: Option<f64>,
    /// Bytes of streamed text/thinking received this turn, plus the last
    /// arrival instant — the status line's stream-liveness indicator.
    pub(crate) stream_received: u64,
    pub(crate) stream_last_data: Option<std::time::Instant>,
    /// Bytes already attributed to completed steps; the live output
    /// estimate uses only the current step's bytes.
    pub(crate) stream_step_base: u64,
    /// Windowed transfer rate: anchor of the current >=1s window and the
    /// last completed window's bytes/sec.
    stream_rate_anchor: Option<(std::time::Instant, u64)>,
    pub(crate) stream_rate: Option<f64>,
    /// The active root turn has appended everything needed to resume from
    /// session history. Set by `TurnStarted`, cleared before each spawn.
    pub(crate) turn_committed: bool,
    pub(crate) retry_available: bool,
    /// Messages submitted during an active turn, auto-sent in order when
    /// the turn completes — each with whatever was attached when it was
    /// submitted, so waiting for the turn costs the user nothing.
    pub(crate) queued_messages: Vec<ilar::agent::Steer>,
    /// Prompts put aside by Ctrl-S and popped back by the same key on a
    /// blank prompt, newest first — for the half-written thought a more
    /// urgent message would otherwise bulldoze.
    pub(crate) input_stash: Vec<StashedPrompt>,
    /// Ctrl-D on a blank prompt with a stash waiting has been warned
    /// about once; the next one quits. Any other key disarms it.
    pub(crate) quit_armed: bool,
    /// Ctrl-L: clear the backend and repaint everything next frame.
    /// Diff rendering never repaints cells it believes unchanged, so
    /// damage from outside writes to the terminal lingers without this.
    pub(crate) force_full_redraw: bool,
    /// Steers handed to a running turn but not yet delivered. Steering
    /// is fire-and-forget, so an aborted turn drops its receiver and
    /// would lose them silently; these get moved back to the queue —
    /// images included, since the copy here is the only one left.
    pub(crate) pending_steers: Vec<ilar::agent::Steer>,
    /// Active goal: (description, completed rounds). Turns auto-continue
    /// until the model emits GOAL_ACHIEVED or the round cap trips.
    pub(crate) goal: Option<(String, u32)>,
    /// Selection inside the inline slash-completion popup.
    pub(crate) slash_selected: usize,
    pub(crate) question_modal: Option<QuestionModal>,
    pub(crate) pending_manager: Option<PendingManager>,
    /// Where the active modal's rows were drawn last frame; how a click
    /// finds the item it landed on. Rebuilt by every render.
    pub(crate) modal_hit: Option<crate::modals::ModalHit>,
    /// Full ids of models with a configured provider; empty means
    /// unrestricted (tests, bare configs fall back to the catalog).
    pub(crate) available_models: Vec<String>,
    /// A command's one-turn model override `(model, variant)`, armed by
    /// `prepare_prompt` and applied by the spawn block, which is where
    /// resolver and store live. `None` in a pair means "keep current".
    pub(crate) pending_model_override: Option<(Option<String>, Option<String>)>,
    /// What to switch back to when the overridden turn ends.
    pub(crate) model_revert: Option<(String, Option<String>)>,
    /// A command that runs as a background subagent instead of a turn.
    pub(crate) pending_subtask: Option<SubtaskRequest>,
    /// Snapshot of spawner.running_background() for rendering.
    pub(crate) background_running: usize,
    /// Snapshot of the service manager's running count for rendering.
    pub(crate) services_running: usize,
    /// (name, running, detail) rows for the sidebar.
    pub(crate) services_view: Vec<(String, bool, String)>,
    /// Subagents working right now, oldest first, for the sidebar.
    pub(crate) agents_view: Vec<AgentRow>,
    /// Idle-session compaction waiting for the scheduler's operation slot.
    pub(crate) compact_requested: bool,
    /// `/btw` question waiting for an idle moment; consumed by settle.
    pub(crate) aside_requested: Option<String>,
    /// The answered aside, displayed until dismissed.
    pub(crate) aside: Option<AsideModal>,
    pub(crate) search_active: bool,
    pub(crate) search_query: String,
    pub(crate) search_matches: Vec<usize>,
    pub(crate) search_current: usize,
    /// (scroll_top, follow_tail) before the search opened; Esc restores.
    search_saved: Option<(usize, bool)>,
    /// The (revision, width) the current matches were scanned at. A
    /// resize reflows every row without touching the transcript, so
    /// revision alone would leave the highlights on the rows the text
    /// used to occupy.
    pub(crate) search_computed_at: Option<(u64, u16)>,
    pub(crate) scroll_top: usize,
    content_rows: usize,
    pub(crate) viewport_rows: usize,
    pub(crate) follow_tail: bool,
    pub(crate) command_palette: Option<CommandPalette>,
    pub(crate) model_picker: Option<ModelPicker>,
    pub(crate) variant_picker: Option<VariantPicker>,
    pub(crate) session_picker: Option<SessionPicker>,
    /// The cross-session content search, reached from the picker.
    pub(crate) session_search: Option<SessionSearch>,
    pub(crate) turn_picker: Option<TurnPicker>,
    pub(crate) link_picker: Option<LinkPicker>,
    /// The turn picker needs the store; set by /rewind or the palette,
    /// consumed by run_app.
    pub(crate) turn_picker_requested: bool,
    /// A whole-session fork (/fork); consumed by run_app.
    pub(crate) fork_requested: bool,
    /// What this session is about, once it has been named.
    pub(crate) topic: Option<String>,
    pub(crate) help_visible: bool,
    pub(crate) help_scroll: usize,
    /// The full todo list overlay: what the sidebar had no room for.
    pub(crate) todos_visible: bool,
    pub(crate) todos_scroll: usize,
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
    pub(crate) transcript_text_area: Rect,
    pub(crate) transcript_cache: TranscriptRenderCache,
    pub(crate) transcript_hit_targets: Vec<Option<TranscriptHitTarget>>,
    /// Resolved at mouse-down: under a streaming turn the transcript
    /// scrolls between press and release, so a release-time lookup
    /// would hit whatever moved under the cursor.
    transcript_pressed_target: Option<TranscriptHitTarget>,
    /// Pointer position within the transcript, viewport-relative; the
    /// row under it gets the hover underline when it is clickable.
    pub(crate) hover: Option<crate::selection::SelectionPoint>,
    /// Images attached with Ctrl-V, waiting to ride the next fresh turn.
    pub(crate) pending_images: Vec<ilar::session::ImageContent>,
    /// The exited-services disclosure: open?, and where its toggle
    /// line landed on screen last frame (None when not drawn).
    pub(crate) services_show_exited: bool,
    pub(crate) services_exited_hit: Option<Rect>,
    /// The agents panel's "+N more" disclosure: expanded past the
    /// half-height cap?, and where its row landed last frame.
    pub(crate) agents_show_all: bool,
    pub(crate) agents_more_hit: Option<Rect>,
    /// Where each agent-panel row line landed last frame and where a
    /// click on it navigates. Rebuilt by every render, like the
    /// disclosure rects beside it.
    pub(crate) agents_row_hits: Vec<(Rect, AgentTarget)>,
    /// A child session's transcript taken over the screen, if any.
    pub(crate) focus: Option<FocusView>,
    /// Raw pointer position for chrome outside the transcript (the
    /// sidebar toggle); the transcript keeps its own relative hover.
    pub(crate) hover_screen: Option<(u16, u16)>,
    pub(crate) transcript_cells: Vec<RenderedRow>,
    pub(crate) transcript_selection: Option<TranscriptSelection>,
    selecting_transcript: bool,
    clipboard: Option<arboard::Clipboard>,
    next_tool_group: u64,
    next_thought: u64,
    pub(crate) expanded_tool_groups: std::collections::HashSet<String>,
    pub(crate) transcript_revision: u64,
    pending_subagent_activity: std::collections::VecDeque<ilar::subagent::SubagentActivity>,
    pub(crate) todos: std::sync::Arc<std::sync::Mutex<ilar::todo::TodoList>>,
}

/// Advance a >=1s measurement window; returns the completed window's
/// bytes/sec when one elapses.
pub(crate) fn windowed_rate(
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
            turn_committed: false,
            retry_available: false,
            queued_messages: Vec::new(),
            input_stash: Vec::new(),
            quit_armed: false,
            force_full_redraw: false,
            pending_steers: Vec::new(),
            goal: None,
            slash_selected: 0,
            question_modal: None,
            pending_manager: None,
            modal_hit: None,
            available_models: Vec::new(),
            pending_model_override: None,
            model_revert: None,
            pending_subtask: None,
            background_running: 0,
            services_running: 0,
            services_view: Vec::new(),
            agents_view: Vec::new(),
            compact_requested: false,
            aside_requested: None,
            aside: None,
            search_active: false,
            search_query: String::new(),
            search_matches: Vec::new(),
            search_current: 0,
            search_saved: None,
            search_computed_at: None,
            scroll_top: 0,
            content_rows: 0,
            viewport_rows: 0,
            follow_tail: true,
            command_palette: None,
            model_picker: None,
            variant_picker: None,
            session_picker: None,
            session_search: None,
            turn_picker: None,
            link_picker: None,
            turn_picker_requested: false,
            fork_requested: false,
            topic: None,
            help_visible: false,
            help_scroll: 0,
            todos_visible: false,
            todos_scroll: 0,
            skill_picker: None,
            skills: Vec::new(),
            commands: Vec::new(),
            // Overwritten from config at startup; the default matters for
            // the window before that and for tests.
            theme: theme::ThemeId::default(),
            theme_picker: None,
            keyboard_enhanced: false,
            model_key_pending: false,
            transcript_text_area: Rect::default(),
            transcript_cache: TranscriptRenderCache::default(),
            transcript_hit_targets: Vec::new(),
            transcript_pressed_target: None,
            hover: None,
            pending_images: Vec::new(),
            services_show_exited: false,
            services_exited_hit: None,
            agents_show_all: false,
            agents_more_hit: None,
            agents_row_hits: Vec::new(),
            focus: None,
            hover_screen: None,
            transcript_cells: Vec::new(),
            transcript_selection: None,
            selecting_transcript: false,
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
        if self.question_modal.is_some() {
            Some(Modal::Question)
        } else if self.pending_manager.is_some() {
            Some(Modal::PendingManager)
        } else if self.help_visible {
            Some(Modal::Help)
        } else if self.todos_visible {
            Some(Modal::Todos)
        } else if self.aside.is_some() {
            Some(Modal::Aside)
        } else if self.theme_picker.is_some() {
            Some(Modal::ThemePicker)
        } else if self.skill_picker.is_some() {
            Some(Modal::SkillPicker)
        } else if self.session_search.is_some() {
            Some(Modal::SessionSearch)
        } else if self.session_picker.is_some() {
            Some(Modal::SessionPicker)
        } else if self.turn_picker.is_some() {
            Some(Modal::TurnPicker)
        } else if self.link_picker.is_some() {
            Some(Modal::LinkPicker)
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
    /// uses: the built-ins, then commands, then skills a command has not
    /// shadowed. Completion, the skill picker and near-match suggestions
    /// all read this, so they cannot disagree about what exists — and a
    /// name a built-in already owns is dropped, because `prepare_prompt`
    /// claims it before any command or skill is consulted.
    pub(crate) fn slash_inventory(&self) -> Vec<(String, String)> {
        let builtin = |name: &str| {
            crate::BUILTIN_SLASH_COMMANDS
                .iter()
                .any(|(builtin, _)| name == *builtin)
        };
        let mut entries: Vec<(String, String)> = crate::BUILTIN_SLASH_COMMANDS
            .iter()
            .map(|(name, description)| ((*name).into(), (*description).into()))
            .collect();
        entries.extend(
            self.commands
                .iter()
                .map(|command| (command.name.clone(), command.description.clone()))
                .chain(
                    self.skills
                        .iter()
                        .filter(|(skill, _)| !self.commands.iter().any(|c| &c.name == skill))
                        .cloned(),
                )
                .filter(|(name, _)| !builtin(name)),
        );
        entries
    }

    /// Give the visible slash-completion popup first refusal on its
    /// navigation and acceptance keys. Returns whether it consumed the key.
    fn handle_slash_completion_key(&mut self, key: KeyEvent) -> bool {
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::SHIFT)
            || !matches!(
                key.code,
                KeyCode::Up | KeyCode::Down | KeyCode::Tab | KeyCode::Enter
            )
        {
            return false;
        }
        let candidates = slash_candidates(self.input.text(), &self.slash_inventory());
        if candidates.is_empty() {
            return false;
        }

        self.slash_selected = self.slash_selected.min(candidates.len() - 1);
        match key.code {
            KeyCode::Up => {
                self.slash_selected =
                    (self.slash_selected + candidates.len() - 1) % candidates.len();
            }
            KeyCode::Down => self.slash_selected = (self.slash_selected + 1) % candidates.len(),
            // The completed input hides the popup, so a second Enter
            // submits it through the normal prompt path.
            KeyCode::Tab | KeyCode::Enter => {
                let (name, _) = &candidates[self.slash_selected];
                // Already typed in full: Enter means send, not "append
                // a space and make me press Enter again". Tab still
                // completes, for reaching the arguments.
                if key.code == KeyCode::Enter && self.input.text() == format!("/{name}") {
                    return false;
                }
                self.input = InputBuffer::from(format!("/{name} "));
                self.slash_selected = 0;
            }
            _ => unreachable!("slash completion keys were filtered above"),
        }
        true
    }

    /// Route prompt-level arrows in precedence order: visible slash
    /// completions, history recall, then transcript scrolling.
    pub(crate) fn handle_prompt_navigation_key(&mut self, key: KeyEvent) -> bool {
        if self.handle_slash_completion_key(key) {
            return true;
        }

        match key.code {
            KeyCode::Up
                if self.history.browsing()
                    || (!self.input.is_multiline() && self.input.is_blank()) =>
            {
                if let Some(text) = self.history.previous(self.input.text()) {
                    self.input = InputBuffer::from(text);
                } else if !self.history.browsing() {
                    self.scroll_up(1);
                }
                true
            }
            KeyCode::Down if self.history.browsing() => {
                if let Some(text) = self.history.next(self.input.text()) {
                    self.input = InputBuffer::from(text);
                }
                true
            }
            // A draft whose cursor still has a row to reach keeps its
            // arrows; at the edges — and in a single-line prompt, which
            // never has one — the transcript takes them rather than
            // letting the key die in the prompt.
            KeyCode::Up if !self.input.can_move_vertical(-1) => {
                self.scroll_up(1);
                true
            }
            KeyCode::Down if !self.input.can_move_vertical(1) => {
                self.scroll_down(1);
                true
            }
            _ => false,
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
            Some(Modal::SessionSearch) => {
                self.session_search.as_mut().unwrap().move_selection(rows);
            }
            Some(Modal::TurnPicker) => {
                self.turn_picker.as_mut().unwrap().move_selection(rows);
            }
            Some(Modal::LinkPicker) => {
                self.link_picker.as_mut().unwrap().move_selection(rows);
            }
            Some(Modal::SkillPicker) => self.skill_picker.as_mut().unwrap().move_selection(rows),
            Some(Modal::CommandPalette) => {
                self.command_palette.as_mut().unwrap().move_selection(rows);
            }
            Some(Modal::Help) => {
                self.help_scroll = self.help_scroll.saturating_add_signed(rows);
            }
            Some(Modal::Todos) => {
                self.todos_scroll = self.todos_scroll.saturating_add_signed(rows);
            }
            Some(Modal::Aside) => {
                let aside = self.aside.as_mut().unwrap();
                aside.scroll = aside.scroll.saturating_add_signed(rows);
            }
            Some(Modal::PendingManager) => {
                let len = self.pending_items().len();
                self.pending_manager
                    .as_mut()
                    .unwrap()
                    .move_selection(rows, len);
            }
            // The question modal navigates its own rows with the arrows;
            // search leaves the wheel to the transcript so results stay
            // browsable underneath it.
            Some(Modal::Question) | Some(Modal::Search) | None => {
                return false;
            }
        }
        true
    }

    /// Route a left click to the overlay in front. Selection only:
    /// activation stays on Enter and dismissal on Esc, because their
    /// cleanup lives in the dispatch arms a mouse event cannot reach.
    /// Returns true when a modal owns the mouse — a miss inside or
    /// outside it is consumed, not passed to the transcript.
    pub(crate) fn click_active_modal(&mut self, column: u16, row: u16) -> bool {
        let Some(modal) = self.active_modal() else {
            return false;
        };
        // Search is a transcript-reading mode; the mouse stays live
        // underneath it.
        if modal == Modal::Search {
            return false;
        }
        if let Some(index) = self
            .modal_hit
            .as_ref()
            .and_then(|hit| hit.item_at(column, row))
        {
            match modal {
                Modal::ModelPicker => self.model_picker.as_mut().unwrap().select(index),
                Modal::VariantPicker => self.variant_picker.as_mut().unwrap().select(index),
                Modal::ThemePicker => {
                    // Like the wheel: the highlighted theme previews live.
                    self.theme_picker.as_mut().unwrap().select(index);
                    self.theme = self.theme_picker.as_ref().unwrap().selected_theme();
                }
                Modal::SessionPicker => self.session_picker.as_mut().unwrap().select(index),
                Modal::SessionSearch => self.session_search.as_mut().unwrap().select(index),
                Modal::TurnPicker => self.turn_picker.as_mut().unwrap().select(index),
                Modal::LinkPicker => self.link_picker.as_mut().unwrap().select(index),
                Modal::SkillPicker => self.skill_picker.as_mut().unwrap().select(index),
                Modal::CommandPalette => self.command_palette.as_mut().unwrap().select(index),
                Modal::PendingManager => {
                    let len = self.pending_items().len();
                    self.pending_manager
                        .as_mut()
                        .expect("pending manager")
                        .select(index, len);
                }
                Modal::Question | Modal::Help | Modal::Todos | Modal::Aside | Modal::Search => {}
            }
        }
        true
    }

    /// Open the link picker over everything currently in the
    /// transcript. Safe at any time: collection is read-only.
    pub(crate) fn open_link_picker(&mut self) {
        self.link_picker = Some(LinkPicker::new(crate::links::collect_links(&self.lines)));
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
        // A session is switched into when nothing is driving it, so
        // whatever it left running died with the process that ran it.
        let restored = restored_session_view_with_store(session, store, Liveness::Settled);
        let from = self.lines.len();
        self.lines.extend(restored.lines);
        self.latest_usage = restored.latest_usage;
        self.session_usage = restored.total_usage;
        self.session_cost = restored.total_cost;
        self.touch_transcript(Some(from));
    }

    /// Record a transcript change: bump the revision, and tell the
    /// render cache the lowest line index whose rows may have moved so
    /// it can leave the rest alone. `None` means no line changed.
    fn touch_transcript(&mut self, from: Option<usize>) {
        self.transcript_revision = self.transcript_revision.wrapping_add(1);
        self.transcript_cache
            .mark_dirty_from(from.unwrap_or(usize::MAX), self.transcript_revision);
    }

    /// The same, for edits whose extent we do not track — the whole
    /// transcript re-renders.
    fn touch_whole_transcript(&mut self) {
        self.touch_transcript(Some(0));
    }

    fn allocate_thought_id(&mut self) -> String {
        next_thought_id(&mut self.next_thought)
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

    pub(crate) fn lines(&self) -> &[Line_] {
        &self.lines
    }

    /// Bring the render cache up to date with the transcript. Lives here
    /// rather than in the render pass because it is the one place that
    /// needs the lines and the cache mutably at once, which only the
    /// module owning both fields can express.
    pub(crate) fn refresh_transcript_cache(&mut self, width: u16, now: std::time::Instant) {
        self.transcript_cache.update(
            &self.lines,
            &self.expanded_tool_groups,
            self.transcript_revision,
            width,
            now,
            self.activity_started,
        );
    }

    pub(crate) fn push_transcript_line(&mut self, line: Line_) {
        self.lines.push(line);
        self.touch_transcript(Some(self.lines.len() - 1));
    }

    pub(crate) fn push_notification(&mut self, description: &str, text: &str) {
        let from = self.lines.len();
        if !self.push_notification_row(text) {
            self.lines
                .push(Line_::System(format!("task notification: {description}")));
            self.lines.push(Line_::User(text.to_string()));
        }
        self.touch_transcript(Some(from));
    }

    /// A message from the user's side of the conversation, however it
    /// got here: typed, queued and auto-sent, or steered into a running
    /// turn. A notification envelope wears its collapsed row; anything
    /// else is a user row with a marker per image. One fold, so the
    /// three arrival paths cannot disagree about what a completion
    /// looks like — including a *typed* envelope, which replay would
    /// collapse too. Returns the first line index it touched.
    pub(crate) fn push_user_message(
        &mut self,
        text: &str,
        images: &[ilar::session::ImageContent],
    ) -> usize {
        let from = self.lines.len();
        if !(images.is_empty() && self.push_notification_row(text)) {
            self.lines
                .push(Line_::User(crate::transcript::user_text_with_images(
                    text, images,
                )));
        }
        self.touch_transcript(Some(from));
        from
    }

    /// The collapsed row a task or tool notification wears, wherever it
    /// arrives — a fresh turn's prompt or a steer into a running one.
    /// False when the text is no notification at all.
    fn push_notification_row(&mut self, text: &str) -> bool {
        if let Some(text) = task_notification_display(text) {
            let id = self.allocate_thought_id();
            self.lines.push(Line_::Task {
                id,
                text,
                expanded: false,
            });
            true
        } else if let Some(text) = tool_notification_display(text) {
            let id = self.allocate_thought_id();
            self.lines.push(Line_::Job {
                id,
                text,
                expanded: false,
            });
            true
        } else {
            false
        }
    }

    /// A `/btw` came back. `Ok(None)` is an abandoned aside — cancelled
    /// or superseded by a newer question — which merits neither a modal
    /// nor a complaint.
    pub(crate) fn finish_aside(
        &mut self,
        question: String,
        result: anyhow::Result<Option<String>>,
    ) {
        match result {
            Ok(Some(answer)) => {
                self.clear_transient_notice();
                self.aside = Some(AsideModal {
                    question,
                    answer,
                    scroll: 0,
                });
            }
            Ok(None) => self.clear_transient_notice(),
            Err(error) => {
                self.set_notice(format!("aside failed: {error:#}"), NoticeLevel::Error);
            }
        }
    }

    pub(crate) fn push_loop_event(&mut self, event: &LoopEvent) {
        // Any loop event is life: it ends a provider-retry notice and a
        // stall-watchdog one alike (except the retry event that sets
        // the former). The watchdog's notices are persistent — nothing
        // transient may bury them — so data arriving is the one thing
        // that takes them down.
        if !matches!(event, LoopEvent::ProviderRetry { .. })
            && self.notice.as_ref().is_some_and(|notice| {
                (!notice.persistent && notice.text.starts_with("provider retry:"))
                    || notice.text.starts_with("provider silent for")
                    || notice.text.starts_with("stall watchdog:")
            })
        {
            self.notice = None;
        }
        // Life also feeds the stall clock itself: a retry cycle or a
        // finishing tool is not provider silence, even though neither
        // streams content bytes. Only re-seed a running clock — the
        // spawn seeds it, and an unwatched pass must stay unwatched.
        if self.stream_last_data.is_some() {
            self.stream_last_data = Some(std::time::Instant::now());
        }
        let touched = self.apply_loop_event(event);
        self.touch_transcript(touched);
    }

    /// Fold a loop event into the transcript and the session status it
    /// implies. The model edits themselves live in `transcript`, shared
    /// with the nested timeline under an agent row; what is left here is
    /// this session's own business — status text, notices, stream
    /// accounting. Returns the lowest line index whose rendering
    /// changed, so the render cache can leave everything above it alone.
    fn apply_loop_event(&mut self, event: &LoopEvent) -> Option<usize> {
        match event {
            // Shown when the loop delivers it, not when it was typed —
            // the transcript reflects what the model actually saw.
            LoopEvent::Steered { text, images } => {
                if let Some(index) = self
                    .pending_steers
                    .iter()
                    .position(|held| &held.text == text)
                {
                    self.pending_steers.remove(index);
                }
                // A steered task result wears the same collapsed row it
                // gets on a fresh turn — never its raw envelope. Only a
                // human steer is a user row: the words, then a marker
                // per image.
                let from = self.push_user_message(text, images);
                self.follow_tail = true;
                Some(from)
            }
            LoopEvent::TurnStarted => {
                self.turn_committed = true;
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
                None
            }
            LoopEvent::TextDelta(t) => {
                self.note_stream_data(t.len());
                let closed = complete_open_thought(&mut self.lines);
                self.status = "responding".into();
                self.set_activity(Activity::Responding);
                let appended = append_text_delta(&mut self.lines, t);
                Some(closed.unwrap_or(appended))
            }
            // Raw thinking and provider-written summaries are the same
            // thing to the transcript: both accumulate into one
            // expandable Thought line (bounded to a tail) so the user
            // can watch the model work live.
            LoopEvent::ThinkingDelta(delta) | LoopEvent::ReasoningSummaryDelta(delta) => {
                self.note_stream_data(delta.len());
                self.status = "thinking".into();
                self.set_activity(Activity::Thinking);
                let next = &mut self.next_thought;
                Some(append_thought_delta(&mut self.lines, delta, || {
                    next_thought_id(next)
                }))
            }
            LoopEvent::ReasoningSummaryCompleted => complete_open_thought(&mut self.lines),
            LoopEvent::ToolStarted { id, name } => {
                let closed = complete_open_thought(&mut self.lines);
                let group = format!("live:{}", self.next_tool_group);
                let started = push_tool_row(&mut self.lines, id, group, name);
                self.status = format!("running {name}");
                self.set_activity(Activity::Tools);
                Some(closed.unwrap_or(started))
            }
            LoopEvent::ToolArguments {
                id,
                arguments: summary,
            } => set_tool_arguments(&mut self.lines, id, summary),
            LoopEvent::ToolInputProgress {
                id,
                received_bytes,
                last_data,
            } => {
                self.stream_last_data = Some(*last_data);
                note_tool_input_progress(&mut self.lines, id, *received_bytes, *last_data)
            }
            LoopEvent::ToolInputComplete { id, arguments } => {
                complete_tool_input(&mut self.lines, id, arguments)
            }
            LoopEvent::SubagentConfigured {
                id,
                description,
                agent,
                model,
            } => configure_subagent_row(&mut self.lines, id, agent, model, description),
            LoopEvent::ToolExecutionStarted {
                id,
                received_bytes,
                started,
            } => start_tool_execution(&mut self.lines, id, *received_bytes, *started),
            LoopEvent::ToolOutputTail { id, tail } => set_tool_tail(&mut self.lines, id, tail),
            LoopEvent::ToolExecutionCompleted { id } => {
                complete_tool_execution(&mut self.lines, id)
            }
            LoopEvent::ToolFinished {
                id,
                name,
                is_error,
                result,
                child_session_id,
            } => {
                let finished =
                    finish_tool_row(&mut self.lines, id, *is_error, result, child_session_id);
                let touched = finished.unwrap_or_else(|| {
                    // A result for a call whose row never arrived still
                    // has to land somewhere.
                    let group = format!("live:{}", self.next_tool_group);
                    let index = push_tool_row(&mut self.lines, id, group, name);
                    finish_tool_row(&mut self.lines, id, *is_error, result, child_session_id);
                    index
                });
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
                Some(touched)
            }
            LoopEvent::ProviderRetry {
                attempt,
                max_retries,
                delay,
                error,
            } => {
                self.status = format!("retrying provider ({attempt}/{max_retries})");
                self.set_activity(Activity::Thinking);
                self.set_notice(
                    format!("provider retry: {error} — in {delay:?}"),
                    NoticeLevel::Warning,
                );
                None
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
                    "{stop_reason} · in {} out {} ({})",
                    usage.input_tokens,
                    usage.output_tokens,
                    Self::cache_hit_display(usage)
                );
                None
            }
            LoopEvent::Compacted {
                context_tokens,
                summary,
            } => {
                self.context_used = *context_tokens;
                self.context_estimated = true;
                self.lines
                    .push(Line_::System(format!("transcript compacted\n{summary}")));
                Some(self.lines.len() - 1)
            }
            LoopEvent::TurnDone { outcome } => {
                let touched = if *outcome == TurnOutcome::Aborted {
                    self.close_open_rows();
                    Some(0)
                } else {
                    prune_incomplete_thoughts(&mut self.lines)
                };
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
                touched
            }
        }
    }

    pub(crate) fn push_subagent_activity(&mut self, activity: &ilar::subagent::SubagentActivity) {
        let touched = apply_subagent_activity(&mut self.lines, &self.session_id, activity);
        if touched.is_none()
            // A UI-spawned subtask has no parent tool call, so its
            // activity can never attach to a Tool row; buffering it
            // would fill the retry queue with entries that stay
            // forever and crowd out ones that could still attach.
            && !activity.parent_call_id.is_empty()
            && self.pending_subagent_activity.len() < 256
        {
            self.pending_subagent_activity.push_back(activity.clone());
        }
        self.touch_transcript(touched);
    }

    /// Route a subagent event into the focus view, when one is open —
    /// beside `push_subagent_activity`, never instead of it: the root
    /// transcript's nested previews keep folding regardless. The
    /// focused session's own events fold flat, exactly as its nested
    /// timeline would; anything deeper nests through the same fold the
    /// root uses, with the focused session as root.
    pub(crate) fn push_focus_activity(&mut self, activity: &ilar::subagent::SubagentActivity) {
        let Some(focus) = self.focus.as_mut() else {
            return;
        };
        if activity.child_session_id == focus.session_id {
            apply_child_loop_event(
                &mut focus.lines,
                &mut focus.group,
                &activity.parent_call_id,
                &activity.event,
            );
            // Follows every event, both ways: a `task_message` can
            // resume a finished session, and a footer still saying
            // "finished" over a streaming transcript would lie. The
            // view itself never vanishes under the reader.
            focus.running = !matches!(activity.event, LoopEvent::TurnDone { .. });
            if !focus.running {
                // The turn is over, so whatever it left open is over
                // too. The seed keeps a live agent's rows open on
                // purpose — this is what closes them when the agent
                // stops without settling them itself.
                close_running_tools(&mut focus.lines);
            }
            focus.touch();
        } else if apply_subagent_activity(&mut focus.lines, &focus.session_id, activity).is_some() {
            focus.touch();
        }
    }

    pub(crate) fn close_focus(&mut self) {
        self.focus = None;
    }

    pub(crate) fn retry_subagent_activity(&mut self) {
        let pending = self.pending_subagent_activity.len();
        for _ in 0..pending {
            let Some(activity) = self.pending_subagent_activity.pop_front() else {
                break;
            };
            match apply_subagent_activity(&mut self.lines, &self.session_id, &activity) {
                Some(index) => self.touch_transcript(Some(index)),
                None => self.pending_subagent_activity.push_back(activity),
            }
        }
    }

    /// Everything a turn that ended badly left mid-flight. Whatever
    /// would have closed these rows is gone with the turn, so an idle
    /// app would otherwise spin over work that has already stopped.
    pub(crate) fn close_open_rows(&mut self) {
        prune_incomplete_thoughts(&mut self.lines);
        close_running_tools(&mut self.lines);
        self.touch_whole_transcript();
    }

    pub(crate) fn finish_turn(&mut self, result: anyhow::Result<TurnOutcome>) {
        self.retry_subagent_activity();
        // A turn that ended cleanly leaves the transcript exactly as the
        // last event did — the point where it is longest is the worst
        // possible moment to throw the rendered rows away.
        let mut touched = None;
        if let Err(error) = result {
            // Closes the open rows and marks the whole transcript.
            self.close_open_rows();
            let mut message = format!("error: {error:#}");
            self.lines.push(Line_::System(message.clone()));
            touched = Some(self.lines.len() - 1);
            if self.turn_committed {
                self.retry_available = true;
                message.push_str(" — Ctrl-R to resume");
            }
            self.set_notice(&message, NoticeLevel::Error);
            self.status = "error".into();
            self.set_activity(Activity::Error);
        }
        self.touch_transcript(touched);
        self.busy = false;
        self.turn_committed = false;
    }

    /// End the goal loop and record it. `None` when no goal was running,
    /// so a caller can say so in whatever way suits it.
    pub(crate) fn abort_goal(&mut self) -> Option<String> {
        let (goal, round) = self.goal.take()?;
        let message = format!("goal aborted after {round} round(s): {goal}");
        self.push_transcript_line(Line_::System(message.clone()));
        Some(message)
    }

    /// Task/tool results that left the notification machinery and now
    /// wait as ordinary texts — spliced into the queue at turn end or
    /// steered but never read. They are still undelivered in the
    /// outbox's eyes, so the quit warning must count them: durable, not
    /// lost, but silent until the next open.
    pub(crate) fn undelivered_queued_results(&self) -> usize {
        self.queued_messages
            .iter()
            .chain(self.pending_steers.iter())
            .filter(|message| is_notification_envelope(&message.text))
            .count()
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

    /// Labels for the pending manager, armed-state confirmations baked
    /// in, so the renderer never needs `&App`.
    pub(crate) fn pending_snapshot(&self) -> Option<crate::modals::PendingSnapshot> {
        let manager = self.pending_manager.as_ref()?;
        let items = self.pending_items();
        let selected = manager.selected().min(items.len().saturating_sub(1));
        let mut armed = false;
        let rows = items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let is_armed = manager.armed == Some(*item) && index == selected;
                armed |= is_armed;
                match item {
                    PendingItem::Queued(queue_index) => {
                        let message = self.queued_messages.get(*queue_index);
                        // A queued task/tool result is not a message the
                        // user wrote: label it for what it is, and let
                        // its delete confirmation say what deleting
                        // costs — nothing lost, only deferred, because
                        // the outbox redelivers at the next open.
                        match message.and_then(queued_result_headline) {
                            Some(_) if is_armed => format!(
                                "task result {}: press d again to delete — the outbox redelivers it when this session next opens",
                                queue_index + 1
                            ),
                            Some(headline) => {
                                format!("task result {}: {}", queue_index + 1, headline)
                            }
                            None => format!(
                                "message {}: {}",
                                queue_index + 1,
                                message
                                    .map(crate::transcript::pending_summary)
                                    .unwrap_or_default()
                            ),
                        }
                    }
                    PendingItem::Goal => {
                        let (goal, round) = self.goal.as_ref().expect("goal item implies goal");
                        if is_armed {
                            format!(
                                "goal (round {round}/{MAX_GOAL_ROUNDS}): press d again to abort"
                            )
                        } else {
                            format!("goal (round {round}/{MAX_GOAL_ROUNDS}): {goal}")
                        }
                    }
                    PendingItem::BackgroundJobs => {
                        if is_armed {
                            format!(
                                "background jobs ({}): press d again to cancel all",
                                self.background_running
                            )
                        } else {
                            format!("background jobs: {} running", self.background_running)
                        }
                    }
                    PendingItem::Services => {
                        if is_armed {
                            format!(
                                "services ({}): press d again to stop all",
                                self.services_running
                            )
                        } else {
                            format!("services: {} running", self.services_running)
                        }
                    }
                    PendingItem::Retry => "resume failed turn from current context".into(),
                }
            })
            .collect();
        Some(crate::modals::PendingSnapshot {
            selected,
            armed,
            rows,
        })
    }

    pub(crate) fn pending_manager_key(&mut self, code: KeyCode, control: bool) -> PendingAction {
        let items = self.pending_items();
        // Computed before the manager borrow: which queued entries are
        // task/tool results, whose deletion needs the armed
        // confirmation below.
        let queued_results: Vec<bool> = self
            .queued_messages
            .iter()
            .map(|message| is_notification_envelope(&message.text))
            .collect();
        let Some(manager) = self.pending_manager.as_mut() else {
            return PendingAction::Stay;
        };
        if items.is_empty() {
            return match code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => PendingAction::Close,
                _ => PendingAction::Stay,
            };
        }
        manager.clamp(items.len());
        let selected = items[manager.selected()];
        if let Some(delta) = crate::modals::nav_delta(code, control) {
            manager.move_selection(delta, items.len());
            return PendingAction::Stay;
        }
        match (code, control) {
            (KeyCode::Esc, _) | (KeyCode::Char('q'), true | false) => PendingAction::Close,
            (KeyCode::Delete | KeyCode::Backspace | KeyCode::Char('d'), _) => {
                match selected {
                    // Removing one queued message the user wrote is
                    // targeted enough to fire immediately. A queued
                    // task result is not one: it falls through to the
                    // armed confirmation, whose label says deletion
                    // only defers redelivery to the next session open.
                    PendingItem::Queued(index)
                        if !queued_results.get(index).copied().unwrap_or(false) =>
                    {
                        PendingAction::DeleteQueued(index)
                    }
                    PendingItem::Retry => PendingAction::DismissRetry,
                    // Goal, background jobs and task results are
                    // investments: confirm.
                    armed_item => {
                        if manager.armed == Some(armed_item) {
                            manager.armed = None;
                            match armed_item {
                                PendingItem::Queued(index) => PendingAction::DeleteQueued(index),
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
    /// The rows may lag the model (the cache renders at draw time), so
    /// the computed revision stays unset: the next frame recomputes
    /// against rows the same frame just rebuilt.
    pub(crate) fn search_refresh(&mut self) {
        self.search_matches = self.transcript_cache.matching_rows(&self.search_query);
        self.search_computed_at = None;
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
        // A focus view in front is what the wheel is pointed at.
        if let Some(focus) = self.focus.as_mut() {
            focus.scroll_by(rows);
            return;
        }
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
        // A held button pins the viewport: rows must not move under a
        // click or a drag-selection while the stream grows the tail.
        if self.follow_tail && !self.selecting_transcript {
            self.scroll_top = max_scroll;
        } else {
            self.scroll_top = self.scroll_top.min(max_scroll);
        }
    }

    pub(crate) fn clear_transcript_selection(&mut self) {
        self.transcript_selection = None;
        self.selecting_transcript = false;
        self.transcript_pressed_target = None;
    }

    pub(crate) fn update_hover(&mut self, column: u16, row: u16) {
        self.hover = selection_point(self.transcript_text_area, column, row, false);
        self.hover_screen = Some((column, row));
    }

    /// A click on the exited-services disclosure; false when it missed
    /// (the transcript gets the click instead).
    pub(crate) fn click_exited_services(&mut self, column: u16, row: u16) -> bool {
        let hit = self
            .services_exited_hit
            .is_some_and(|rect| rect.contains(ratatui::layout::Position::new(column, row)));
        if hit {
            self.services_show_exited = !self.services_show_exited;
        }
        hit
    }

    /// A click on the agents panel's "+N more" disclosure; false when
    /// it missed (the transcript gets the click instead).
    pub(crate) fn click_agents_more(&mut self, column: u16, row: u16) -> bool {
        let hit = self
            .agents_more_hit
            .is_some_and(|rect| rect.contains(ratatui::layout::Position::new(column, row)));
        if hit {
            self.agents_show_all = !self.agents_show_all;
        }
        hit
    }

    /// Where a click on the agents panel navigates; `None` when it
    /// missed every row (the transcript gets the click instead). The
    /// caller acts — this only reads the map the render left.
    pub(crate) fn click_agent_row(&self, column: u16, row: u16) -> Option<AgentTarget> {
        let position = ratatui::layout::Position::new(column, row);
        self.agents_row_hits
            .iter()
            .find(|(rect, _)| rect.contains(position))
            .map(|(_, target)| target.clone())
    }

    pub(crate) fn begin_transcript_selection(&mut self, column: u16, row: u16) {
        self.clear_transcript_selection();
        let Some(point) = selection_point(self.transcript_text_area, column, row, false) else {
            return;
        };
        self.transcript_pressed_target = self
            .transcript_hit_targets
            .get(point.row)
            .cloned()
            .flatten();
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
        self.update_transcript_selection(column, row);
    }

    pub(crate) fn finish_transcript_selection(&mut self, column: u16, row: u16) -> Option<String> {
        if !self.selecting_transcript {
            return None;
        }
        self.update_transcript_selection(column, row);
        self.selecting_transcript = false;
        let pressed = self.transcript_pressed_target.take();
        let selection = self.transcript_selection?;
        // What makes a press a click is where it ended, not whether the
        // pointer ever moved: a trackpad emits drag events for a tap,
        // and a press that drifted a cell — or drifted and came back —
        // is still someone clicking a disclosure.
        if press_was_a_click(selection) {
            self.transcript_selection = None;
            if let Some(target) = pressed {
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
        // Every row below the toggled one moves and every row above it
        // stays exactly where it was, so the cache is marked from the
        // row itself. Marking from zero — which is what `lines_mut`
        // does — re-parses the markdown, re-wraps and re-highlights the
        // entire session for one click, which is what made expanding a
        // long transcript feel stuck. A row we cannot locate falls back
        // to the whole transcript: over-invalidating costs work, never
        // correctness.
        let toggled = match target {
            TranscriptHitTarget::ToolGroup(id) => {
                if !self.expanded_tool_groups.remove(&id) {
                    self.expanded_tool_groups.insert(id.clone());
                }
                tool_group_index(&self.lines, &id)
            }
            TranscriptHitTarget::Tool(id) => toggle_tool_expansion(&mut self.lines, &id),
            TranscriptHitTarget::Thought(id) => toggle_note_expansion(&mut self.lines, &id),
        };
        self.touch_transcript(Some(toggled.unwrap_or(0)));
    }

    /// Attach an image to the next fresh turn, or say why not: mid-turn
    /// attachments would ride steering (text-only), and a model without
    /// vision would silently ignore what the user thinks it saw.
    /// Returns whether it attached, so multi-file drops can summarize.
    pub(crate) fn attach_image(&mut self, image: ilar::session::ImageContent) -> bool {
        /// Decoded payload cap; providers reject far larger, but a session
        /// line this size is already unpleasant to carry around.
        const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
        if self.busy {
            self.set_notice(
                "a turn is running — images send with a fresh message; try again when it ends",
                NoticeLevel::Warning,
            );
            return false;
        }
        if !ilar::model::supports_vision(&self.current_model) {
            self.set_notice(
                format!("{} cannot view images", self.current_model),
                NoticeLevel::Warning,
            );
            return false;
        }
        let bytes = image.byte_len();
        if bytes > MAX_IMAGE_BYTES {
            self.set_notice(
                format!(
                    "image too large ({} — the cap is {})",
                    crate::text::format_bytes(bytes as u64),
                    crate::text::format_bytes(MAX_IMAGE_BYTES as u64)
                ),
                NoticeLevel::Warning,
            );
            return false;
        }
        let kind = image
            .media_type
            .strip_prefix("image/")
            .unwrap_or(&image.media_type)
            .to_string();
        self.pending_images.push(image);
        self.set_notice(
            format!(
                "image attached ({kind} · {}) — sends with your next message, Esc discards",
                crate::text::format_bytes(bytes as u64)
            ),
            NoticeLevel::Info,
        );
        true
    }

    /// A dropped image file: sniffed, bounded, attached — or a notice
    /// saying why not. Returns whether it attached.
    pub(crate) fn attach_image_file(&mut self, path: &std::path::Path) -> bool {
        match std::fs::read(path) {
            Ok(bytes) => match ilar::image::from_file_bytes(&bytes) {
                Some(image) => return self.attach_image(image),
                None => self.set_notice(
                    format!(
                        "{} is not a supported image (png, jpeg, webp, gif)",
                        path.display()
                    ),
                    NoticeLevel::Warning,
                ),
            },
            Err(error) => self.set_notice(
                format!("cannot read {}: {error}", path.display()),
                NoticeLevel::Error,
            ),
        }
        false
    }

    /// The clipboard's image, PNG-encoded; `Ok(None)` when it holds none.
    pub(crate) fn read_clipboard_image(&mut self) -> Result<Option<ilar::session::ImageContent>> {
        if self.clipboard.is_none() {
            self.clipboard = Some(arboard::Clipboard::new().context("opening clipboard")?);
        }
        let image = match self
            .clipboard
            .as_mut()
            .expect("clipboard initialized")
            .get_image()
        {
            Ok(image) => image,
            Err(arboard::Error::ContentNotAvailable) => return Ok(None),
            Err(error) => return Err(error).context("reading clipboard image"),
        };
        let (width, height, pixels) = match ilar::image::downscale_rgba(
            image.width,
            image.height,
            &image.bytes,
            ilar::image::MAX_IMAGE_DIM,
        ) {
            Some((width, height, pixels)) => (width, height, std::borrow::Cow::from(pixels)),
            None => (image.width, image.height, image.bytes),
        };
        let png = ilar::image::encode_png(width as u32, height as u32, &pixels)
            .context("encoding clipboard image")?;
        Ok(Some(ilar::session::ImageContent::png(&png)))
    }

    /// Copy, by whichever route can reach the person's own clipboard.
    ///
    /// Over SSH there is no local clipboard to open: the display
    /// variables are unset and arboard's X11 probe blocks until it times
    /// out, which is a frozen UI ending in an error. OSC 52 hands the
    /// text to the terminal emulator instead, so it lands on the
    /// clipboard of the machine the person is sitting at — which is the
    /// one they meant, even when ilar is running somewhere else.
    pub(crate) fn copy_to_clipboard(&mut self, text: &str) -> Result<()> {
        if prefer_terminal_clipboard() {
            return copy_via_terminal(text);
        }
        let native = self.copy_natively(text);
        match native {
            Ok(()) => Ok(()),
            // A clipboard that exists but refuses is still worth a
            // second try through the terminal; the error that matters
            // is the one from the route that was actually available.
            Err(error) => copy_via_terminal(text).map_err(|_| error),
        }
    }

    fn copy_natively(&mut self, text: &str) -> Result<()> {
        if self.clipboard.is_none() {
            self.clipboard = Some(arboard::Clipboard::new().context("opening clipboard")?);
        }
        self.clipboard
            .as_mut()
            .expect("clipboard initialized")
            .set_text(text.to_string())
            .context("writing clipboard")
    }

    /// Ctrl-S, both directions: a draft — text, attached images or both
    /// — is put aside so a quick message can go first; an empty prompt
    /// with nothing attached takes the newest stash back, images
    /// included. The input title shows a count while anything waits.
    pub(crate) fn stash_or_pop_input(&mut self) {
        if self.input.is_blank() && self.pending_images.is_empty() {
            match self.input_stash.pop() {
                Some(stashed) => {
                    self.input = crate::input::InputBuffer::from(stashed.text);
                    self.pending_images = stashed.images;
                    self.end_history_browsing();
                    self.clear_transient_notice();
                }
                None => self.set_notice("nothing stashed", NoticeLevel::Info),
            }
        } else {
            self.input_stash.push(StashedPrompt {
                text: self.input.take(),
                images: std::mem::take(&mut self.pending_images),
            });
            self.end_history_browsing();
            self.set_notice(
                format!(
                    "input stashed ({}) · Ctrl-S on a blank prompt pops",
                    self.input_stash.len()
                ),
                NoticeLevel::Info,
            );
        }
    }

    /// Drop any history-recall cursor. Stashing empties the prompt
    /// without submitting, and popping refills it from somewhere else;
    /// either way the recall position is stale, and leaving it standing
    /// makes the next Up overwrite what was typed since. `push` of an
    /// empty prompt is history's reset — it clears cursor and draft and
    /// records nothing.
    fn end_history_browsing(&mut self) {
        self.history.push("");
    }

    /// Ctrl-D on a blank prompt is the exit, but a blank prompt is
    /// exactly what a waiting stash — which dies with the process —
    /// and undelivered task results — which survive it, in the outbox
    /// — look like. Warn once, naming both costs at once so the
    /// second press is the answer to everything said; the repeat
    /// quits. `None` means quit now.
    pub(crate) fn quit_warning(&mut self, undelivered: usize) -> Option<String> {
        if (self.input_stash.is_empty() && undelivered == 0)
            || std::mem::take(&mut self.quit_armed)
        {
            return None;
        }
        self.quit_armed = true;
        let mut parts = Vec::new();
        if !self.input_stash.is_empty() {
            parts.push(format!(
                "{} stashed prompt(s) would be lost (Ctrl-S pops them)",
                self.input_stash.len()
            ));
        }
        if undelivered > 0 {
            parts.push(format!(
                "{undelivered} task result(s) are undelivered and will arrive next time this session opens"
            ));
        }
        Some(format!("{} — Ctrl-D again quits", parts.join("; ")))
    }

    pub(crate) fn set_notice(&mut self, text: impl Into<String>, level: NoticeLevel) {
        self.set_notice_with_lifetime(text, level, level == NoticeLevel::Error);
    }

    pub(crate) fn set_persistent_notice(&mut self, text: impl Into<String>, level: NoticeLevel) {
        self.set_notice_with_lifetime(text, level, true);
    }

    /// The stall watchdog's notice claims the line only from nothing,
    /// from transients, or from an earlier notice of its own. A standing
    /// persistent reminder — paused notifications, an error — outranks
    /// it: the watchdog would first bury the reminder and then destroy
    /// it, since any loop event clears stall notices outright.
    pub(crate) fn set_stall_notice(&mut self, text: impl Into<String>) {
        let may_claim = self.notice.as_ref().is_none_or(|notice| {
            !notice.persistent
                || notice.text.starts_with("provider silent for")
                || notice.text.starts_with("stall watchdog:")
        });
        if may_claim {
            self.set_notice_with_lifetime(text, NoticeLevel::Warning, true);
        }
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

    /// "cache 77%" — the share of the last request's prompt the provider
    /// served from its cache. Two raw counts left the reader dividing in
    /// their head, and cache writes are not reported at all on some
    /// backends, so a pair of numbers was as likely to mislead as inform.
    pub(crate) fn cache_hit_display(usage: &ilar::session::Usage) -> String {
        let prompt = usage
            .input_tokens
            .saturating_add(usage.cache_read_input_tokens)
            .saturating_add(usage.cache_creation_input_tokens);
        match cache_share(prompt, usage.cache_read_input_tokens) {
            Some(share) => format!("cache {share}%"),
            None => "cache —".to_string(),
        }
    }
}

/// Whether to skip this host's own clipboard and ask the terminal.
///
/// Two cases, and the second is the one that bites. A Linux or BSD
/// session with no display has nothing to open. But a session that
/// arrived over SSH usually *does* name a display — forwarded, or a
/// stale `:0` left in the environment — and reaching for it is worse
/// than useless: at best the text lands on a clipboard nobody is
/// looking at, at worst the X11 connect blocks until it times out and
/// the UI freezes for the duration. Either way the clipboard the person
/// means is at their end of the connection, which is where OSC 52 puts
/// it.
fn prefer_terminal_clipboard() -> bool {
    let named = |key| std::env::var_os(key).is_some_and(|value| !value.is_empty());
    if named("SSH_CONNECTION") || named("SSH_TTY") {
        return true;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        !named("DISPLAY") && !named("WAYLAND_DISPLAY")
    }
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    {
        false
    }
}

/// Ask the terminal to put `text` on its own clipboard (OSC 52). The
/// bytes travel the same connection the session does, so this reaches
/// the operator's machine rather than the host's.
///
/// Not every terminal obeys — some ship it disabled — and there is no
/// reply to wait for, so a success here means the request was sent, not
/// that it was honoured. That is the honest limit of the protocol.
fn copy_via_terminal(text: &str) -> Result<()> {
    use std::io::Write as _;

    let sequence = osc52_sequence(text, std::env::var_os("TMUX").is_some());
    let mut out = std::io::stdout().lock();
    out.write_all(sequence.as_bytes())
        .and_then(|()| out.flush())
        .context("asking the terminal to copy")
}

/// The OSC 52 request itself. Inside tmux it has to be wrapped in tmux's
/// passthrough with the inner ESC doubled, or tmux swallows an escape it
/// does not recognise instead of forwarding it.
fn osc52_sequence(text: &str, inside_tmux: bool) -> String {
    use base64::Engine as _;

    let payload = base64::engine::general_purpose::STANDARD.encode(text);
    if inside_tmux {
        format!("\x1bPtmux;\x1b\x1b]52;c;{payload}\x07\x1b\\")
    } else {
        format!("\x1b]52;c;{payload}\x07")
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
            // The content search is the front door; ^G inside it
            // reaches the classic list. Needs nothing at open — the
            // scan starts with the first keystroke.
            app.session_search = Some(SessionSearch::new());
        }
        PaletteCommand::Links => {
            app.open_link_picker();
        }
        PaletteCommand::Rewind => {
            // Turns are loaded by the caller (needs the store); the
            // palette only records the request.
            app.turn_picker_requested = true;
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
            app.set_notice("compaction starting", NoticeLevel::Info);
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

/// The image-file paths a terminal drop pastes, if that is what the
/// pasted text is: one line of shell-style words (quotes and
/// backslash escapes honored, as the common terminals emit them), all
/// with image extensions. One stray word and the paste is text.
pub(crate) fn dropped_image_paths(text: &str) -> Option<Vec<std::path::PathBuf>> {
    let words = split_shell_words(text.trim())?;
    let is_image = |word: &String| {
        let lower = word.to_lowercase();
        ["png", "jpg", "jpeg", "webp", "gif"]
            .iter()
            .any(|ext| lower.ends_with(&format!(".{ext}")))
    };
    (!words.is_empty() && words.iter().all(is_image))
        .then(|| words.into_iter().map(std::path::PathBuf::from).collect())
}

/// One line into shell-style words; `None` on newlines or dangling
/// quotes/escapes — those pastes are prose, not paths.
fn split_shell_words(text: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\n' => return None,
            '\\' => current.push(chars.next()?),
            '\'' | '"' => loop {
                match chars.next() {
                    Some('\n') | None => return None,
                    Some(inner) if inner == c => break,
                    Some(inner) => current.push(inner),
                }
            },
            ' ' => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    Some(words)
}

/// Close every tool row a turn left open, at any depth.
///
/// An abort drops the parent's tool futures and an error or a crash
/// takes the channel with it, so a running subagent never reports
/// back — no `TurnDone` activity arrives to clear `child_running`, and
/// the agent row would spin forever. A shallow sweep is not enough:
/// the child's own rows are nested inside it, and `child_running`
/// masks the parent row's state while it is set.
/// Click-target ids for expandable thought rows.
fn next_thought_id(counter: &mut u64) -> String {
    *counter = counter.wrapping_add(1);
    format!("thought:{counter}")
}

fn close_running_tools(lines: &mut [Line_]) {
    for line in lines {
        if let Line_::Tool {
            state,
            child_running,
            child_lines,
            ..
        } = line
        {
            if matches!(*state, ToolState::Running | ToolState::Complete) {
                *state = ToolState::Failed;
            }
            *child_running = false;
            close_running_tools(child_lines);
        }
    }
}

/// How far a press may drift between button-down and button-up and
/// still count as a click. A real selection is a word or a line; a
/// single cell either way is nobody's intent, and a trackpad reports
/// exactly that for a firm tap. Deciding on the distance rather than on
/// whether a drag event ever arrived is what makes expanding a row
/// reliable: the old rule made every tap a coin flip.
const CLICK_SLOP_CELLS: usize = 1;

fn press_was_a_click(selection: TranscriptSelection) -> bool {
    selection.anchor.row.abs_diff(selection.focus.row) <= CLICK_SLOP_CELLS
        && selection.anchor.column.abs_diff(selection.focus.column) <= CLICK_SLOP_CELLS
}

#[cfg(test)]
mod tests {
    use super::*;

    use ratatui::style::{Modifier, Style};
    use unicode_width::UnicodeWidthStr;

    use crate::ERROR;
    use crate::input::input_accepts_keys;
    use crate::selection::{RenderedCell, highlight_transcript_selection, transcript_cells};
    use crate::text::wrap_styled_line;
    use crate::view::{activity_line, stream_liveness};

    #[test]
    fn ctrl_s_stashes_the_prompt_and_pops_it_back_newest_first() {
        let mut app = App::new();
        app.input = crate::input::InputBuffer::from("half-written thought");
        app.stash_or_pop_input();
        assert!(app.input.is_blank());
        assert_eq!(
            app.input_stash,
            vec![StashedPrompt {
                text: "half-written thought".to_string(),
                images: Vec::new(),
            }]
        );
        assert!(app.notice.is_some(), "stashing says where the text went");

        app.input = crate::input::InputBuffer::from("second stash");
        app.stash_or_pop_input();

        app.stash_or_pop_input();
        assert_eq!(app.input.text(), "second stash");
        assert_eq!(app.input.cursor(), "second stash".len());
        app.input.clear();
        app.stash_or_pop_input();
        assert_eq!(app.input.text(), "half-written thought");
        assert!(app.input_stash.is_empty());

        // Nothing left: the prompt stays blank and the status says why.
        app.input.clear();
        app.clear_notice();
        app.stash_or_pop_input();
        assert!(app.input.is_blank());
        assert!(app.notice.is_some());
    }

    /// The stall warning claims the notice line only from nothing, from
    /// transients, or from itself. Burying a standing persistent
    /// reminder would destroy it: any loop event clears stall notices
    /// outright, taking whatever they replaced down with them.
    #[test]
    fn the_stall_warning_does_not_bury_a_standing_persistent_notice() {
        let mut app = App::new();
        app.set_persistent_notice(
            "notifications paused; send a message to resume",
            NoticeLevel::Info,
        );
        app.set_stall_notice("provider silent for 300s — Esc aborts, the turn will retry-resume");
        assert_eq!(
            app.notice.as_ref().unwrap().text,
            "notifications paused; send a message to resume"
        );

        // A transient yields, and the warning then climbs over itself.
        app.clear_notice();
        app.set_notice("copied to clipboard", NoticeLevel::Info);
        app.set_stall_notice("provider silent for 300s — Esc aborts, the turn will retry-resume");
        assert!(
            app.notice
                .as_ref()
                .unwrap()
                .text
                .starts_with("provider silent for 300s"),
        );
        app.set_stall_notice("provider silent for 310s — Esc aborts, the turn will retry-resume");
        assert!(app.notice.as_ref().unwrap().text.contains("310"));
        app.set_stall_notice("stall watchdog: provider silent for 600s — aborting the turn");
        assert!(app.notice.as_ref().unwrap().text.starts_with("stall watchdog:"));
    }

    /// A retry cycle or a finishing tool is not provider silence: any
    /// drained loop event re-seeds a running stall clock. A clock that
    /// never started stays stopped — an unwatched pass is unwatched.
    #[test]
    fn any_loop_event_re_seeds_a_running_stall_clock() {
        let mut app = App::new();
        let long_ago = std::time::Instant::now() - std::time::Duration::from_secs(400);
        app.stream_last_data = Some(long_ago);
        app.push_loop_event(&LoopEvent::ReasoningSummaryCompleted);
        assert!(
            app.stream_last_data.unwrap().elapsed() < std::time::Duration::from_secs(1),
            "a loop event is life"
        );

        app.stream_last_data = None;
        app.push_loop_event(&LoopEvent::ReasoningSummaryCompleted);
        assert!(app.stream_last_data.is_none());
    }

    /// `task_message` resumes a subagent, so its row is a subagent row —
    /// but it cannot say so up front the way `task` does, because the
    /// agent's name is behind the task id. The first sign of a child is
    /// what settles it; before this, the row rendered as a plain tool
    /// and hid the very work it had started.
    #[test]
    fn a_tool_that_turns_out_to_have_a_child_becomes_an_agent_row() {
        let mut app = App::new();
        app.session_id = "root".into();
        app.push_loop_event(&LoopEvent::ToolStarted {
            id: "msg-1".into(),
            name: "task_message".into(),
        });

        app.push_subagent_activity(&ilar::subagent::SubagentActivity {
            parent_session_id: "root".into(),
            parent_call_id: "msg-1".into(),
            child_session_id: "child".into(),
            agent: "build".into(),
            event: LoopEvent::ThinkingDelta("re-reading the parser".into()),
        });

        let row = app
            .lines
            .iter()
            .find_map(|line| match line {
                Line_::Tool {
                    kind, child_lines, ..
                } => Some((kind, child_lines)),
                _ => None,
            })
            .expect("the task_message row");
        assert!(
            matches!(row.0, ToolKind::Agent { name, .. } if name == "build"),
            "{:?}",
            row.0
        );
        assert!(!row.1.is_empty(), "the child's work is shown, not hidden");
    }

    /// Running over SSH there is no clipboard on this machine worth
    /// reaching; OSC 52 asks the terminal to use the one in front of
    /// the person instead.
    #[test]
    fn a_terminal_copy_carries_the_text_to_whoever_is_watching() {
        let plain = super::osc52_sequence("hello", false);
        assert_eq!(plain, "\x1b]52;c;aGVsbG8=\x07");

        // tmux forwards a foreign escape only through its passthrough.
        let wrapped = super::osc52_sequence("hello", true);
        assert!(
            wrapped.starts_with("\x1bPtmux;\x1b\x1b]52;c;"),
            "{wrapped:?}"
        );
        assert!(wrapped.ends_with("\x07\x1b\\"), "{wrapped:?}");
        assert!(wrapped.contains("aGVsbG8="), "{wrapped:?}");
    }

    #[test]
    fn a_stash_carries_its_attached_images_and_gives_them_back() {
        let mut app = App::new();
        let attached = ilar::session::ImageContent::png(b"first");
        app.input = crate::input::InputBuffer::from("look at this");
        app.pending_images = vec![attached.clone()];
        app.stash_or_pop_input();

        // The next message must not inherit the stashed prompt's images.
        assert!(
            app.pending_images.is_empty(),
            "images ride with the text they were attached to"
        );
        app.input = crate::input::InputBuffer::from("something urgent");
        app.input.clear();

        app.stash_or_pop_input();
        assert_eq!(app.input.text(), "look at this");
        assert_eq!(app.pending_images, vec![attached]);
    }

    #[test]
    fn an_image_only_draft_stashes_instead_of_popping() {
        let mut app = App::new();
        app.pending_images = vec![ilar::session::ImageContent::png(b"screenshot")];
        app.input_stash.push(StashedPrompt {
            text: "older".into(),
            images: Vec::new(),
        });
        app.stash_or_pop_input();

        assert_eq!(app.input_stash.len(), 2, "the attachment was put aside");
        assert!(app.pending_images.is_empty());
        assert_eq!(app.input_stash[1].images.len(), 1);
        assert!(app.input_stash[1].text.is_empty());
    }

    #[test]
    fn stashing_ends_history_recall_so_the_next_up_arrow_recalls_afresh() {
        let mut app = App::new();
        app.history.push("an old prompt");
        app.handle_prompt_navigation_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.input.text(), "an old prompt");

        app.stash_or_pop_input();
        assert!(!app.history.browsing(), "a blank prompt is not mid-recall");

        // Freshly typed text after the stash must survive an Up: with a
        // live cursor, history would replace it instead of stashing a
        // draft first.
        app.input = crate::input::InputBuffer::from("typed after stashing");
        app.handle_prompt_navigation_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.input.text(), "typed after stashing");
    }

    #[test]
    fn ctrl_d_warns_once_before_quitting_on_a_waiting_stash() {
        let mut app = App::new();
        assert_eq!(app.quit_warning(0), None, "no stash, no ceremony");

        app.input = crate::input::InputBuffer::from("half-written thought");
        app.stash_or_pop_input();
        let warning = app.quit_warning(0).expect("the first Ctrl-D warns");
        assert!(warning.contains('1'), "{warning}");
        assert_eq!(app.quit_warning(0), None, "the second Ctrl-D quits anyway");

        // Consuming the arm resets it: a Ctrl-D much later warns again
        // (the dispatcher disarms on every other key for the same
        // reason).
        assert!(!app.quit_armed);
        assert!(app.quit_warning(0).is_some());
    }

    #[test]
    fn ctrl_d_names_undelivered_results_and_the_stash_in_one_warning() {
        let mut app = App::new();
        let warning = app.quit_warning(2).expect("undelivered results warn");
        assert!(warning.contains("2 task result(s)"), "{warning}");
        assert!(warning.contains("next time"), "{warning}");
        assert_eq!(app.quit_warning(2), None, "the second Ctrl-D quits");

        // Both costs in one message: the second press answers both.
        app.input = crate::input::InputBuffer::from("half-written thought");
        app.stash_or_pop_input();
        let warning = app.quit_warning(1).expect("both warn together");
        assert!(warning.contains("stashed"), "{warning}");
        assert!(warning.contains("task result"), "{warning}");
        assert_eq!(app.quit_warning(1), None);
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
    use crate::drain_wheel_batch;
    use crate::input::{PromptAction, handle_prompt_key, slash_candidates};
    use crate::modals::{CommandPaletteAction, PALETTE_COMMANDS, is_command_palette_shortcut};
    use crate::selection::SelectionPoint;
    use crate::session_view::restored_session_view;
    use crate::text::tests::rendered_text;
    use crate::transcript::{
        ToolKind, ToolProgress, reasoning_summary_title, tool_line, transcript_entry_lines,
    };
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

        app.question_modal = Some(QuestionModal::new(ilar::question::QuestionRequest {
            questions: vec![ilar::question::Question {
                id: "confirm".into(),
                prompt: "Continue?".into(),
                description: None,
                required: true,
                kind: ilar::question::QuestionKind::SingleChoice {
                    allow_other: false,
                    options: vec![ilar::question::QuestionOption {
                        id: "yes".into(),
                        label: "Yes".into(),
                        description: None,
                    }],
                },
            }],
        }));
        assert_eq!(app.active_modal(), Some(Modal::Question));
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
        assert_eq!(app.model_picker.as_ref().unwrap().nav.selected, 0);

        assert!(app.scroll_active_modal(3));
        assert_eq!(app.model_picker.as_ref().unwrap().nav.selected, 3);
        assert!(app.scroll_active_modal(-3));
        assert_eq!(app.model_picker.as_ref().unwrap().nav.selected, 0);
    }

    /// The pending manager grew a scrolling list, but the wheel was
    /// still declining it on the grounds that it was "a handful of
    /// rows" — so the modal moved under the arrows and sat still under
    /// the wheel.
    #[test]
    fn the_wheel_moves_the_pending_manager_selection() {
        let mut app = App::new();
        app.queued_messages = vec!["one".into(), "two".into(), "three".into()];
        app.pending_manager = Some(PendingManager::default());
        assert_eq!(app.pending_manager.as_ref().unwrap().selected(), 0);

        assert!(app.scroll_active_modal(2));
        assert_eq!(app.pending_manager.as_ref().unwrap().selected(), 2);
        assert!(app.scroll_active_modal(-1));
        assert_eq!(app.pending_manager.as_ref().unwrap().selected(), 1);

        // An empty list has nothing to move to and must not index it.
        app.queued_messages.clear();
        assert!(app.scroll_active_modal(1));
        assert_eq!(app.pending_manager.as_ref().unwrap().selected(), 0);
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

    /// A click on a picker row selects that row: the render pass
    /// records where the rows landed, and the click maps back through
    /// it. The click coordinates come from the *rendered buffer*, not
    /// from the hit map being validated, so a row map that drifts from
    /// what is actually drawn fails here.
    #[test]
    fn a_click_selects_the_picker_row_it_lands_on() {
        let mut app = App::new();
        let models: Vec<_> = ilar::model::catalog().iter().take(5).collect();
        let first = models[0].full_id();
        let expected = models[3].full_id();
        app.model_picker = Some(ModelPicker::new(models, &first));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();

        let screen_row = |terminal: &ratatui::Terminal<ratatui::backend::TestBackend>,
                          needle: &str| {
            (0..30u16)
                .find(|row| {
                    (0..100u16)
                        .map(|column| terminal.backend().buffer()[(column, *row)].symbol())
                        .collect::<String>()
                        .contains(needle)
                })
                .unwrap_or_else(|| panic!("{needle:?} is on screen"))
        };
        let hit = app.modal_hit.clone().expect("picker records a hit map");

        // Click the row the buffer shows the target model on.
        let target_row = screen_row(&terminal, &expected);
        assert!(app.click_active_modal(hit.area.x, target_row));
        assert_eq!(app.model_picker.as_ref().unwrap().nav.selected, 3);
        assert_eq!(
            app.model_picker
                .as_mut()
                .unwrap()
                .handle_key(KeyCode::Enter, false),
            crate::modals::PickerAction::Choose(expected),
            "Enter must act on the clicked row"
        );

        // A click on the search header, located the same way, moves
        // nothing.
        let header_row = screen_row(&terminal, "type to filter");
        assert!(app.click_active_modal(hit.area.x, header_row));
        assert_eq!(app.model_picker.as_ref().unwrap().nav.selected, 3);

        // A click outside the modal is consumed, not passed through to
        // the transcript underneath.
        assert!(app.click_active_modal(0, 0));
        assert_eq!(app.model_picker.as_ref().unwrap().nav.selected, 3);

        // The clamp is the stale-map defence: an index past the list
        // must land on the last entry, not panic or run off.
        app.model_picker.as_mut().unwrap().select(999);
        assert_eq!(app.model_picker.as_ref().unwrap().nav.selected, 4);
    }

    /// Clicking a theme previews it, exactly like the wheel and the
    /// arrow keys: the footer advertises live preview.
    #[test]
    fn a_click_previews_the_theme_it_selects() {
        let mut app = App::new();
        app.theme = theme::ThemeId::ALL[0];
        app.theme_picker = Some(ThemePicker::new(app.theme));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();

        let hit = app.modal_hit.clone().expect("hit map");
        let target_row = hit
            .rows
            .iter()
            .position(|item| *item == Some(1))
            .expect("second theme drawn") as u16;
        assert!(app.click_active_modal(hit.area.x, hit.area.y + target_row));
        assert_eq!(
            app.theme,
            theme::ThemeId::ALL[1],
            "the click moved the marker without previewing"
        );

        // With search open the transcript keeps the mouse.
        app.theme_picker = None;
        app.open_search();
        assert!(!app.click_active_modal(10, 10));
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
                cwd: None,
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::UserMessage {
                id: new_id(),
                text: "hello".into(),
                images: Vec::new(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::UserMessage {
                id: new_id(),
                text: "<task-notification>\nTask \"Assess architecture and risks\" completed.\n<result>\nRepository review\n</result>\n</task-notification>".into(),
                images: Vec::new(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::UserMessage {
                id: new_id(),
                text: "<tool-notification>\nBackground job job-1 (\"Run checks\") completed.\n<result>\nchecks passed\n</result>\n</tool-notification>".into(),
                images: Vec::new(),
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
    fn long_input_wraps_and_grows_to_show_the_whole_message() {
        let backend = ratatui::backend::TestBackend::new(40, 12);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.input = InputBuffer::from(
            "first section of a long message then the second section remains visible",
        );

        terminal.draw(|frame| app.render(frame)).unwrap();

        let screen = (0..terminal.backend().buffer().area.height)
            .map(|row| {
                (0..terminal.backend().buffer().area.width)
                    .map(|column| terminal.backend().buffer()[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            screen.contains("first section of a long message then"),
            "{screen}"
        );
        assert!(
            screen.contains("the second section remains visible"),
            "{screen}"
        );
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

    /// A resize reflows every row without touching the transcript, so
    /// matches keyed on the revision alone kept pointing at the rows the
    /// text used to occupy — highlights on the wrong lines, jumps
    /// landing beside the hit.
    #[test]
    fn a_resize_recomputes_the_search_matches() {
        let mut app = App::new();
        app.lines = (0..6)
            .map(|index| {
                Line_::User(format!(
                    "filler line {index} carrying enough words to wrap several times once the \
                     terminal is made narrow"
                ))
            })
            .chain(std::iter::once(Line_::Assistant(
                "the special needle answer".into(),
            )))
            .collect();

        let mut wide =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 40)).unwrap();
        wide.draw(|frame| app.render(frame)).unwrap();
        app.open_search();
        app.search_query = "needle".into();
        app.search_refresh();
        wide.draw(|frame| app.render(frame)).unwrap();
        let wide_matches = app.search_matches.clone();
        assert_eq!(wide_matches.len(), 1, "{wide_matches:?}");

        let mut narrow =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 40)).unwrap();
        narrow.draw(|frame| app.render(frame)).unwrap();

        assert_ne!(
            app.search_matches, wide_matches,
            "the resize left the match on the row it used to be on"
        );
    }

    #[test]
    fn slash_completion_arrow_keys_move_and_wrap_the_selection() {
        let mut app = App::new();
        app.skills = vec![
            ("deploy".to_string(), "Deploy things".to_string()),
            ("greptile".to_string(), "Review comments".to_string()),
        ];
        app.input = InputBuffer::from("/");
        let candidates = slash_candidates(app.input.text(), &app.slash_inventory());
        let plain = |code| KeyEvent::new(code, KeyModifiers::NONE);

        assert!(app.handle_prompt_navigation_key(plain(KeyCode::Down)));
        assert_eq!(app.slash_selected, 1);

        assert!(app.handle_prompt_navigation_key(plain(KeyCode::Up)));
        assert_eq!(app.slash_selected, 0);

        assert!(app.handle_prompt_navigation_key(plain(KeyCode::Up)));
        assert_eq!(app.slash_selected, candidates.len() - 1);

        assert!(app.handle_prompt_navigation_key(plain(KeyCode::Down)));
        assert_eq!(app.slash_selected, 0);

        assert!(app.handle_prompt_navigation_key(plain(KeyCode::Down)));
        assert!(app.handle_prompt_navigation_key(plain(KeyCode::Enter)));
        assert_eq!(app.input.text(), format!("/{} ", candidates[1].0));
        assert_eq!(app.slash_selected, 0);

        app.input = InputBuffer::from("/go");
        assert!(
            !app.handle_prompt_navigation_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT,))
        );
        assert_eq!(app.input.text(), "/go");
    }

    #[test]
    fn prompt_arrows_keep_history_behavior_without_slash_completions() {
        let mut app = App::new();
        app.history.push("previous prompt");

        assert!(app.handle_prompt_navigation_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE,)));
        assert_eq!(app.input.text(), "previous prompt");

        assert!(
            app.handle_prompt_navigation_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE,))
        );
        assert!(app.input.is_blank());
    }

    /// The F1 help promises Up / Down scroll the transcript, and the
    /// dispatcher runs navigation before the prompt editor. At the top
    /// and bottom of a multiline draft the cursor has no row to reach,
    /// so the arrow belongs to the transcript — everywhere else it has
    /// to fall through and move the cursor instead.
    #[test]
    fn multiline_edge_arrows_scroll_while_mid_draft_arrows_reach_the_prompt() {
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let mut app = App::new();
        app.content_rows = 100;
        app.viewport_rows = 20;
        app.scroll_top = 50;
        app.input = InputBuffer::from("one\ntwo\nthree");

        // Mid-draft: navigation declines and the prompt editor moves the
        // cursor, one line per press, without the transcript budging.
        for expected in [7, 3] {
            assert!(!app.handle_prompt_navigation_key(key(KeyCode::Up)));
            assert_eq!(
                handle_prompt_key(&mut app.input, key(KeyCode::Up)),
                PromptAction::Edited
            );
            assert_eq!(app.input.cursor(), expected);
        }
        assert_eq!(app.scroll_top, 50, "a cursor move must not scroll");

        // On the first line the arrow scrolls and leaves the draft alone.
        assert!(app.handle_prompt_navigation_key(key(KeyCode::Up)));
        assert_eq!(app.scroll_top, 49);
        assert_eq!(app.input.text(), "one\ntwo\nthree");
        assert_eq!(app.input.cursor(), 3);

        // The bottom edge is the same story downwards.
        for expected in [7, 11] {
            assert!(!app.handle_prompt_navigation_key(key(KeyCode::Down)));
            assert_eq!(
                handle_prompt_key(&mut app.input, key(KeyCode::Down)),
                PromptAction::Edited
            );
            assert_eq!(app.input.cursor(), expected);
        }
        assert_eq!(app.scroll_top, 49);
        assert!(app.handle_prompt_navigation_key(key(KeyCode::Down)));
        assert_eq!(app.scroll_top, 50);

        // A blank prompt still recalls history rather than scrolling.
        let mut app = App::new();
        app.content_rows = 100;
        app.viewport_rows = 20;
        app.scroll_top = 50;
        app.history.push("previous prompt");
        assert!(app.handle_prompt_navigation_key(key(KeyCode::Up)));
        assert_eq!(app.input.text(), "previous prompt");
        assert_eq!(app.scroll_top, 50, "recall must not scroll");
    }

    #[test]
    fn a_fully_typed_command_submits_on_enter_instead_of_completing() {
        let mut app = App::new();
        app.commands = Vec::new();

        // Partial name: Enter completes, as before.
        app.input = InputBuffer::from("/sess");
        let consumed = app.handle_slash_completion_key(KeyEvent::from(KeyCode::Enter));
        assert!(consumed);
        assert_eq!(app.input.text(), "/sessions ");

        // Exact name: Enter falls through to the submit path; a second
        // Enter must not be needed.
        app.input = InputBuffer::from("/sessions");
        let consumed = app.handle_slash_completion_key(KeyEvent::from(KeyCode::Enter));
        assert!(!consumed, "Enter was eaten by completion");
        assert_eq!(app.input.text(), "/sessions");

        // Tab keeps completing even on an exact match.
        let consumed = app.handle_slash_completion_key(KeyEvent::from(KeyCode::Tab));
        assert!(consumed);
        assert_eq!(app.input.text(), "/sessions ");
    }

    /// One inventory feeds completion, the skill picker and the
    /// near-match suggestions, and it is where the built-ins live: a
    /// skill that shadows a built-in name is unreachable (the built-in
    /// is claimed first), so it must not be offered either.
    #[test]
    fn slash_input_shows_inline_completion_including_builtins() {
        let skills = vec![
            ("deploy".to_string(), "Deploy things".to_string()),
            ("greptile".to_string(), "Review comments".to_string()),
            ("compact".to_string(), "external duplicate".to_string()),
        ];
        let mut app = App::new();
        app.skills = skills;
        let inventory = app.slash_inventory();

        // All candidates on bare slash, fuzzy-filtered as the name grows.
        let all = slash_candidates("/", &inventory);
        assert_eq!(all.len(), 8);
        for builtin in ["goal", "compact", "rewind", "fork", "sessions", "btw"] {
            assert_eq!(
                all.iter().filter(|(name, _)| name == builtin).count(),
                1,
                "{builtin} in {all:?}"
            );
        }
        // The built-in's own wording wins over the shadowing skill's.
        assert!(
            all.iter()
                .any(|(name, description)| name == "compact" && description.contains("session")),
            "{all:?}"
        );
        let filtered = slash_candidates("/go", &inventory);
        assert_eq!(
            filtered.first().map(|(name, _)| name.as_str()),
            Some("goal")
        );
        // Finished name (whitespace) or non-slash input: no popup.
        assert!(slash_candidates("/goal recover", &inventory).is_empty());
        assert!(slash_candidates("plain text", &inventory).is_empty());
        assert!(slash_candidates("/zzz", &inventory).is_empty());

        // The popup renders above the input.
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
    fn pending_snapshot_bakes_labels_and_arming() {
        let mut app = App::new();
        app.queued_messages = vec!["first\nline".into()];
        app.goal = Some(("recover the engine".into(), 2));
        app.pending_manager = Some(PendingManager::default());

        let snapshot = app.pending_snapshot().expect("manager is open");
        assert_eq!(snapshot.selected, 0);
        assert!(!snapshot.armed);
        assert_eq!(snapshot.rows[0], "message 1: first line");
        assert_eq!(
            snapshot.rows[1],
            format!("goal (round 2/{MAX_GOAL_ROUNDS}): recover the engine")
        );

        // Arming the goal changes its label to the confirmation.
        app.pending_manager_key(KeyCode::Down, false);
        app.pending_manager_key(KeyCode::Char('d'), false);
        let snapshot = app.pending_snapshot().expect("manager is open");
        assert_eq!(snapshot.selected, 1);
        assert!(snapshot.armed);
        assert!(snapshot.rows[1].contains("press d again to abort"));

        app.pending_manager = None;
        assert!(app.pending_snapshot().is_none());
    }

    /// A queued task result is not a message the user wrote: the
    /// manager names it for what it is, headline and all — never its
    /// raw envelope — and deleting it takes a confirmation that says
    /// deletion only defers redelivery to the next session open.
    #[test]
    fn a_queued_task_result_is_labeled_and_its_deletion_deferred_with_confirmation() {
        let mut app = App::new();
        app.queued_messages = vec![
            "<task-notification>\nTask \"bg survey\" completed.\n<result>\nfound it\n</result>\n</task-notification>".into(),
            "ordinary follow-up".into(),
        ];
        app.pending_manager = Some(PendingManager::default());

        let snapshot = app.pending_snapshot().expect("manager is open");
        assert_eq!(snapshot.rows[0], "task result 1: bg survey completed.");
        assert!(!snapshot.rows[0].contains("<task-notification>"));
        assert_eq!(snapshot.rows[1], "message 2: ordinary follow-up");

        // First d arms; the label becomes the deferred-not-lost
        // confirmation; the second d fires.
        assert_eq!(
            app.pending_manager_key(KeyCode::Char('d'), false),
            PendingAction::Stay
        );
        let snapshot = app.pending_snapshot().expect("manager is open");
        assert!(snapshot.armed);
        assert!(
            snapshot.rows[0].contains("press d again")
                && snapshot.rows[0].contains("redelivers it when this session next opens"),
            "{}",
            snapshot.rows[0]
        );
        assert_eq!(
            app.pending_manager_key(KeyCode::Char('d'), false),
            PendingAction::DeleteQueued(0)
        );

        // The ordinary message keeps its immediate, targeted delete.
        app.queued_messages.remove(0);
        assert_eq!(
            app.pending_manager_key(KeyCode::Char('d'), false),
            PendingAction::DeleteQueued(0)
        );
    }

    /// A tool-notification envelope wears the same treatment: the job
    /// headline, not the raw tag.
    #[test]
    fn a_queued_job_result_is_labeled_by_its_headline() {
        let mut app = App::new();
        app.queued_messages = vec![
            "<tool-notification>\nBackground job job-1 (\"Run checks\") completed.\n<result>\nchecks passed\n</result>\n</tool-notification>".into(),
        ];
        app.pending_manager = Some(PendingManager::default());

        let snapshot = app.pending_snapshot().expect("manager is open");
        assert_eq!(
            snapshot.rows[0],
            "task result 1: job-1 (\"Run checks\") completed."
        );
    }

    /// The quit warning's undelivered count reaches results that left
    /// the notification machinery: envelope texts spliced into the
    /// message queue or steered but never read. Ordinary messages and
    /// steers stay out of it.
    #[test]
    fn queued_and_steered_task_results_count_as_undelivered() {
        let mut app = App::new();
        assert_eq!(app.undelivered_queued_results(), 0);
        app.queued_messages = vec![
            "ordinary follow-up".into(),
            "<task-notification>\nTask \"bg survey\" completed.\n</task-notification>".into(),
        ];
        app.pending_steers = vec![
            "go left".into(),
            "<tool-notification>\nBackground job job-1 (\"Run checks\") completed.\n</tool-notification>".into(),
        ];

        assert_eq!(app.undelivered_queued_results(), 2);
    }

    #[test]
    fn pending_manager_lists_and_mutates_standing_state() {
        let mut app = App::new();
        app.queued_messages = vec!["first".into(), "second".into()];
        app.goal = Some(("recover the engine".into(), 2));
        app.background_running = 1;
        app.retry_available = true;
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

    /// Navigation wraps over the whole list — the list is longer than
    /// the modal, so the far end is only reachable by wrapping.
    #[test]
    fn pending_manager_navigation_wraps_over_the_whole_list() {
        let mut app = App::new();
        app.queued_messages = (0..20)
            .map(|index| format!("message {index}").into())
            .collect();
        app.pending_manager = Some(PendingManager::default());

        app.pending_manager_key(KeyCode::Up, false);
        assert_eq!(
            app.pending_snapshot().expect("manager is open").selected,
            19,
            "Up from the top wraps to the last item"
        );
        app.pending_manager_key(KeyCode::Down, false);
        assert_eq!(
            app.pending_snapshot().expect("manager is open").selected,
            0,
            "Down from the last item wraps to the top"
        );
    }

    /// Hit maps go stale the moment the pending list changes under
    /// them, so a click can name a row that no longer exists.
    #[test]
    fn a_stale_pending_click_lands_on_the_last_row() {
        let mut app = App::new();
        app.queued_messages = (0..20)
            .map(|index| format!("message {index}").into())
            .collect();
        app.pending_manager = Some(PendingManager::default());
        app.modal_hit = Some(crate::modals::ModalHit {
            area: Rect::new(0, 0, 10, 1),
            rows: vec![Some(99)],
        });

        assert!(app.click_active_modal(0, 0));
        assert_eq!(
            app.pending_snapshot().expect("manager is open").selected,
            19
        );
    }

    #[test]
    fn services_show_what_runs_and_summarize_the_dead() {
        let mut app = App::new();
        // One running buried under five exited: the old capped list hid
        // the only row that mattered.
        app.services_view = vec![
            ("build".into(), false, "exit 1".into()),
            ("compile".into(), false, "exit 1".into()),
            ("compile2".into(), false, "exit 1".into()),
            ("compile3".into(), false, "exit 1".into()),
            ("lint".into(), false, "exit 1".into()),
            ("web".into(), true, "up 3m2s".into()),
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
        assert!(!screen.contains("compile"), "{screen}");
        assert!(screen.contains("5 exited"), "{screen}");
        assert!(screen.contains("todos"), "{screen}");
    }

    #[test]
    fn clicking_the_exited_count_reveals_and_folds_the_dead_services() {
        let mut app = App::new();
        app.services_view = vec![
            ("web".into(), true, "up 3m2s".into()),
            ("build".into(), false, "exit 1".into()),
            ("lint".into(), false, "exit 2".into()),
        ];
        app.services_running = 1;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 30)).unwrap();
        let screen = |terminal: &ratatui::Terminal<ratatui::backend::TestBackend>| {
            (0..30)
                .map(|row| {
                    (0..140)
                        .map(|column| terminal.backend().buffer()[(column, row)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        terminal.draw(|frame| app.render(frame)).unwrap();
        assert!(
            screen(&terminal).contains("▸ 2 exited"),
            "{}",
            screen(&terminal)
        );
        assert!(!screen(&terminal).contains("build"));
        let hit = app.services_exited_hit.expect("toggle rect recorded");

        // Click reveals the dead with their details…
        assert!(app.click_exited_services(hit.x, hit.y));
        terminal.draw(|frame| app.render(frame)).unwrap();
        assert!(screen(&terminal).contains("▾ 2 exited"));
        assert!(screen(&terminal).contains("build · exit 1"));
        assert!(screen(&terminal).contains("lint · exit 2"));

        // …a second click folds them away.
        assert!(app.click_exited_services(hit.x, hit.y));
        terminal.draw(|frame| app.render(frame)).unwrap();
        assert!(!screen(&terminal).contains("build"));

        // A miss is not consumed, and no panel means no rect.
        assert!(!app.click_exited_services(0, 0));
        app.services_view.clear();
        terminal.draw(|frame| app.render(frame)).unwrap();
        assert!(app.services_exited_hit.is_none());
    }

    #[test]
    fn the_header_names_the_session_once_it_has_a_topic() {
        let mut app = App::new();
        let screen = |app: &mut App| {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 10)).unwrap();
            terminal.draw(|frame| app.render(frame)).unwrap();
            (0..10)
                .map(|row| {
                    (0..90)
                        .map(|column| terminal.backend().buffer()[(column, row)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        // Untitled sessions look exactly as they did before.
        assert!(screen(&mut app).contains("┌ilar"), "{}", screen(&mut app));

        app.topic = Some("flaky auth test".into());
        let titled = screen(&mut app);
        assert!(titled.contains("ilar · flaky auth test"), "{titled}");
    }

    #[test]
    fn running_agents_show_in_the_sidebar_until_they_finish() {
        let mut app = App::new();
        app.agents_view = vec![
            AgentRow {
                session_id: "child-survey".into(),
                depth: 0,
                description: "survey the picker core".into(),
                agent: "explore".into(),
                background: false,
                delivering: false,
                foreign_parent: None,
                elapsed: std::time::Duration::from_secs(72),
            },
            AgentRow {
                session_id: "child-index".into(),
                depth: 1,
                description: "rebuild the index".into(),
                agent: "build".into(),
                background: true,
                delivering: false,
                foreign_parent: None,
                elapsed: std::time::Duration::from_secs(5),
            },
        ];
        let screen = |app: &mut App| {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 30)).unwrap();
            terminal.draw(|frame| app.render(frame)).unwrap();
            (0..30)
                .map(|row| {
                    (0..140)
                        .map(|column| terminal.backend().buffer()[(column, row)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        let running = screen(&mut app);
        assert!(running.contains("agents (2)"), "{running}");
        // The map leads with the place you came from…
        assert!(running.contains("● main"), "{running}");
        assert!(running.contains("survey the picker core"), "{running}");
        assert!(running.contains("explore · 1m 12s"), "{running}");
        // …and a child of a child indents under its parent.
        assert!(running.contains("▸   rebuild the index"), "{running}");
        // Background work is marked; foreground is the default.
        assert!(running.contains("build · bg · 5s"), "{running}");
        // The panel never crowds out what it sits above.
        assert!(running.contains("todos"), "{running}");

        // Stacked with everything else the sidebar can carry, the
        // panels share the space instead of evicting each other.
        app.goal = Some(("finish the sidebar work".into(), 2));
        app.services_view = vec![("web".into(), true, "up 3m2s".into())];
        app.services_running = 1;
        let stacked = screen(&mut app);
        for section in ["goal 2/", "agents (2)", "services (1)", "todos"] {
            assert!(stacked.contains(section), "missing {section}: {stacked}");
        }

        // Nothing running, no panel: the sidebar is not a museum.
        app.agents_view.clear();
        let idle = screen(&mut app);
        assert!(!idle.contains("agents ("), "{idle}");
        assert!(idle.contains("todos"), "{idle}");
    }

    /// The agents panel hides nothing until space runs out — and when
    /// it must, the "+N more" row is a real disclosure: click to spend
    /// the todo list's space on the full roster, click to fold it
    /// back, and the expansion lets go by itself once everyone fits
    /// the ordinary cap again.
    #[test]
    fn the_agents_more_row_expands_the_panel_and_folds_it_back() {
        let mut app = App::new();
        app.agents_view = (0..8)
            .map(|index| AgentRow {
                session_id: format!("child-{index}"),
                depth: 0,
                description: format!("hunt bug number {index}"),
                agent: "explore".into(),
                background: false,
                delivering: false,
                foreign_parent: None,
                elapsed: std::time::Duration::from_secs(30),
            })
            .collect();
        let screen = |app: &mut App| {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 30)).unwrap();
            terminal.draw(|frame| app.render(frame)).unwrap();
            (0..30)
                .map(|row| {
                    (0..140)
                        .map(|column| terminal.backend().buffer()[(column, row)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        // Overflow: the head shows, the tail is counted, the count is
        // a row of its own that recorded where it landed.
        let collapsed = screen(&mut app);
        assert!(collapsed.contains("hunt bug number 0"), "{collapsed}");
        assert!(!collapsed.contains("hunt bug number 7"), "{collapsed}");
        assert!(collapsed.contains("▸ +"), "{collapsed}");
        let hit = app.agents_more_hit.expect("more rect recorded");

        // Click: the whole roster, at the todo list's expense, with
        // the way back in its place.
        assert!(app.click_agents_more(hit.x, hit.y));
        let expanded = screen(&mut app);
        assert!(expanded.contains("hunt bug number 7"), "{expanded}");
        assert!(expanded.contains("▾ show less"), "{expanded}");

        // A second click folds it back.
        let hit = app.agents_more_hit.expect("less rect recorded");
        assert!(app.click_agents_more(hit.x, hit.y));
        let refolded = screen(&mut app);
        assert!(!refolded.contains("hunt bug number 7"), "{refolded}");
        assert!(refolded.contains("▸ +"), "{refolded}");

        // A miss is not consumed. And once the roster fits the cap,
        // a stale expansion releases itself: no disclosure at all.
        assert!(!app.click_agents_more(0, 0));
        app.agents_show_all = true;
        app.agents_view.truncate(2);
        let fits = screen(&mut app);
        assert!(!fits.contains("show less"), "{fits}");
        assert!(!app.agents_show_all, "expansion released");
        assert!(app.agents_more_hit.is_none());

        // An expansion does not outlive its roster: everyone finishing
        // must not leave the next batch pre-expanded.
        app.agents_show_all = true;
        app.agents_view.clear();
        let _ = screen(&mut app);
        assert!(!app.agents_show_all, "expansion died with the roster");
    }

    /// The panel rows are a click map: every drawn line of an agent
    /// names its session, "main" names the way home, and a miss stays
    /// a miss so the transcript keeps its clicks.
    #[test]
    fn clicking_an_agent_row_names_where_it_navigates() {
        let mut app = App::new();
        app.agents_view = vec![
            AgentRow {
                session_id: "child-a".into(),
                depth: 0,
                description: "survey the picker core".into(),
                agent: "explore".into(),
                background: false,
                delivering: false,
                foreign_parent: None,
                elapsed: std::time::Duration::from_secs(10),
            },
            AgentRow {
                session_id: "child-b".into(),
                depth: 1,
                description: "rebuild the index".into(),
                agent: "build".into(),
                background: false,
                delivering: false,
                foreign_parent: None,
                elapsed: std::time::Duration::from_secs(5),
            },
        ];
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 30)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();

        // Main leads, then two lines per agent, all recorded.
        assert_eq!(app.agents_row_hits.len(), 5);
        assert_eq!(app.agents_row_hits[0].1, AgentTarget::Main);
        let main_rect = app.agents_row_hits[0].0;
        assert_eq!(
            app.click_agent_row(main_rect.x, main_rect.y),
            Some(AgentTarget::Main)
        );
        // Both lines of a row are the same click.
        for index in [1, 2] {
            let rect = app.agents_row_hits[index].0;
            assert_eq!(
                app.click_agent_row(rect.x + 1, rect.y),
                Some(AgentTarget::Focus("child-a".into()))
            );
        }
        let rect = app.agents_row_hits[3].0;
        assert_eq!(
            app.click_agent_row(rect.x, rect.y),
            Some(AgentTarget::Focus("child-b".into()))
        );
        assert_eq!(app.click_agent_row(0, 0), None);

        // No panel, no map: the rects die with the roster.
        app.agents_view.clear();
        terminal.draw(|frame| app.render(frame)).unwrap();
        assert!(app.agents_row_hits.is_empty());
    }

    fn focus_activity(
        child: &str,
        parent: &str,
        call: &str,
        event: LoopEvent,
    ) -> ilar::subagent::SubagentActivity {
        ilar::subagent::SubagentActivity {
            parent_session_id: parent.into(),
            parent_call_id: call.into(),
            child_session_id: child.into(),
            agent: "explore".into(),
            event,
        }
    }

    /// Focus is a filter over the stream we already have: the focused
    /// session's events fold flat, a grandchild nests under its tool
    /// row, TurnDone marks the ending in place without closing the
    /// view — and the root transcript is byte-identical through the
    /// whole round trip.
    #[test]
    fn a_focus_view_follows_its_session_and_leaves_the_root_untouched() {
        let mut app = App::new();
        app.session_id = "root".into();
        app.push_transcript_line(Line_::System("root business".into()));
        let root_before = app.lines().to_vec();

        app.focus = Some(FocusView::new(
            "child-a".into(),
            "explore · survey the picker core".into(),
            vec![Line_::System("replayed history".into())],
            true,
        ));

        // The focused child's own stream folds flat…
        app.push_focus_activity(&focus_activity(
            "child-a",
            "root",
            "call-1",
            LoopEvent::TextDelta("fresh words".into()),
        ));
        // …a grandchild nests under the tool row that started it…
        app.push_focus_activity(&focus_activity(
            "child-a",
            "root",
            "call-1",
            LoopEvent::ToolStarted {
                id: "tool-9".into(),
                name: "task".into(),
            },
        ));
        app.push_focus_activity(&focus_activity(
            "grandchild",
            "child-a",
            "tool-9",
            LoopEvent::TextDelta("deeper words".into()),
        ));
        // …and an unrelated sibling's stream is not its business.
        app.push_focus_activity(&focus_activity(
            "child-b",
            "root",
            "call-2",
            LoopEvent::TextDelta("someone else".into()),
        ));

        let focus = app.focus.as_ref().expect("focus open");
        assert_eq!(focus.lines[0], Line_::System("replayed history".into()));
        assert_eq!(focus.lines[1], Line_::Assistant("fresh words".into()));
        assert!(
            matches!(
                &focus.lines[2],
                Line_::Tool { id, child_lines, .. }
                    if id == "tool-9"
                        && child_lines
                            .iter()
                            .any(|line| matches!(line, Line_::Assistant(text) if text == "deeper words"))
            ),
            "{:?}",
            focus.lines
        );
        assert!(focus.running);
        assert!(
            !format!("{:?}", focus.lines).contains("someone else"),
            "a sibling leaked into the focus view"
        );

        // The ending is said in place; the view does not vanish.
        app.push_focus_activity(&focus_activity(
            "child-a",
            "root",
            "call-1",
            LoopEvent::TurnDone {
                outcome: TurnOutcome::Completed,
            },
        ));
        assert!(app.focus.as_ref().is_some_and(|focus| !focus.running));

        // The way back, and the root never moved: focus was a view,
        // not a transfer.
        app.close_focus();
        assert!(app.focus.is_none());
        assert_eq!(app.lines(), &root_before[..]);
    }

    /// The focus view takes the transcript area — title, footer and
    /// seeded rows — while the sidebar stays: the agents panel is how
    /// you got here and how you leave. A finished agent changes the
    /// footer, not the screen.
    #[test]
    fn the_focus_view_renders_over_the_transcript_with_an_honest_footer() {
        let mut app = App::new();
        app.push_transcript_line(Line_::Assistant("root prose".into()));
        app.agents_view = vec![AgentRow {
            session_id: "child-a".into(),
            depth: 0,
            description: "survey the picker core".into(),
            agent: "explore".into(),
            background: false,
            delivering: false,
            foreign_parent: None,
            elapsed: std::time::Duration::from_secs(10),
        }];
        app.focus = Some(FocusView::new(
            "child-a".into(),
            "explore · survey the picker core".into(),
            vec![Line_::Assistant("seeded child reply".into())],
            true,
        ));
        let screen = |app: &mut App| {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 30)).unwrap();
            terminal.draw(|frame| app.render(frame)).unwrap();
            (0..30)
                .map(|row| {
                    (0..140)
                        .map(|column| terminal.backend().buffer()[(column, row)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        let focused = screen(&mut app);
        assert!(
            focused.contains("explore · survey the picker core"),
            "{focused}"
        );
        assert!(focused.contains("seeded child reply"), "{focused}");
        assert!(focused.contains("read-only"), "{focused}");
        assert!(!focused.contains("root prose"), "{focused}");
        // The map stays on screen: main is still a click away.
        assert!(focused.contains("● main"), "{focused}");
        assert!(!app.agents_row_hits.is_empty());

        app.focus.as_mut().unwrap().running = false;
        let finished = screen(&mut app);
        assert!(finished.contains("agent finished · Esc returns"), "{finished}");
        assert!(finished.contains("seeded child reply"), "{finished}");

        // Esc's path: the root transcript comes back as it was.
        app.close_focus();
        let returned = screen(&mut app);
        assert!(returned.contains("root prose"), "{returned}");
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
    fn turn_errors_offer_retry_only_after_session_state_is_committed() {
        let mut app = App::new();
        app.finish_turn(Err(anyhow::anyhow!("api down")));
        assert!(!app.retry_available, "nothing was committed to resume");

        let mut app = App::new();
        app.push_loop_event(&LoopEvent::TurnStarted);
        app.finish_turn(Err(anyhow::anyhow!("api down")));
        assert!(app.retry_available);
        let (notice, _) = app.operational_notice().expect("error notice");
        assert!(notice.contains("Ctrl-R to resume"), "{notice}");

        // A fresh successful turn clears nothing prematurely.
        app.retry_available = false;
        app.finish_turn(Ok(TurnOutcome::Completed));
        assert!(!app.retry_available);
    }

    #[test]
    fn compacted_event_displays_the_handover_summary() {
        let mut app = App::new();
        app.push_loop_event(&LoopEvent::Compacted {
            context_tokens: 42,
            summary: "keep the parser decision and pending migration".into(),
        });

        assert!(matches!(
            app.lines.last(),
            Some(Line_::System(text))
                if text.contains("transcript compacted")
                    && text.contains("keep the parser decision and pending migration")
        ));
        let rendered = app
            .transcript_lines(80, std::time::Instant::now())
            .into_iter()
            .map(|line| {
                line.spans
                    .into_iter()
                    .map(|span| span.content)
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("keep the parser decision"), "{rendered}");
    }

    #[test]
    fn provider_retry_event_shows_backoff_activity() {
        let mut app = App::new();
        app.push_loop_event(&LoopEvent::ProviderRetry {
            attempt: 2,
            max_retries: 3,
            delay: std::time::Duration::from_secs(1),
            error: "overloaded".into(),
        });

        assert!(
            app.status.contains("retrying provider (2/3)"),
            "{}",
            app.status
        );
        let (notice, _) = app.operational_notice().expect("retry notice");
        assert!(notice.contains("overloaded"), "{notice}");
        assert!(notice.contains("1s"), "{notice}");

        app.push_loop_event(&LoopEvent::TextDelta("recovered".into()));
        assert!(app.operational_notice().is_none());
    }

    /// The stall watchdog's notices are persistent so nothing transient
    /// buries them — which makes arriving data the one thing that may
    /// take them down.
    #[test]
    fn stream_data_clears_a_stall_watchdog_notice() {
        let mut app = App::new();
        app.push_loop_event(&LoopEvent::TurnStarted);
        app.set_persistent_notice(
            "provider silent for 300s — Esc aborts, the turn will retry-resume",
            NoticeLevel::Warning,
        );
        // Persistent: an ordinary transient notice cannot replace it.
        app.set_notice("something routine", NoticeLevel::Info);
        let (notice, _) = app.operational_notice().expect("stall notice");
        assert!(notice.starts_with("provider silent for"), "{notice}");

        // Data at last — the stall is over, and so is the notice.
        app.push_loop_event(&LoopEvent::TextDelta("data".into()));
        assert!(app.operational_notice().is_none());

        // The abort flavour goes the same way: the TurnDone that the
        // cancellation produces clears it and posts its own word.
        app.set_persistent_notice(
            "stall watchdog: provider silent for 600s — aborting the turn",
            NoticeLevel::Warning,
        );
        app.push_loop_event(&LoopEvent::TurnDone {
            outcome: TurnOutcome::Aborted,
        });
        let (notice, _) = app.operational_notice().expect("abort notice");
        assert_eq!(notice, "turn aborted");
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
        // 1500 read of a 1820-token prompt (300 fresh + 1500 read + 20 written).
        assert!(status.contains("cache 82%"), "{status}");
        assert!(status.contains("Σ 1k"), "{status}");
        assert!(status.contains("$0.004"), "{status}");
        let narrow = rendered_text(&app.status_line(60));
        assert!(narrow.contains("gpt-5.6"), "{narrow}");
        assert!(narrow.contains("high"), "{narrow}");
        assert!(narrow.contains("i300/o50"), "{narrow}");
        assert!(narrow.contains("cache 82%"), "{narrow}");
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
        // Assert in role space: the adaptive theme is the one whose remap
        // is the identity, so the roles survive to the buffer.
        app.theme = theme::ThemeId::Terminal;

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
        // Standalone rows pad nothing: alignment is the group's job
        // (grouped_tool_rows_align_to_their_widest_sibling), and a row
        // with no siblings has nothing to align with.
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
        assert!(short.contains("read ✓ src/main.rs"), "{short}");

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
    fn the_scrollbar_thumb_reaches_both_ends_of_its_track() {
        // The transcript's own right border is the last column; the
        // track sits one column inside it.
        let track = |terminal: &ratatui::Terminal<ratatui::backend::TestBackend>| {
            let buffer = terminal.backend().buffer();
            (0..buffer.area.height)
                .map(|row| buffer[(buffer.area.width - 2, row)].symbol().to_string())
                .filter(|symbol| symbol == "│" || symbol == "┃")
                .collect::<Vec<_>>()
        };

        // Short and tall: the rounding that used to strand the thumb
        // grew with the track.
        for height in [20u16, 30, 40, 50] {
            let backend = ratatui::backend::TestBackend::new(40, height);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            let mut app = App::new();
            app.lines = (0..200)
                .map(|index| Line_::System(format!("row {index}")))
                .collect();

            // Following the tail: the thumb ends flush with the track.
            terminal.draw(|frame| app.render(frame)).unwrap();
            let tail = track(&terminal);
            assert_eq!(app.scroll_top, app.max_scroll(), "height {height}");
            assert_eq!(
                tail.last().map(String::as_str),
                Some("┃"),
                "height {height}: {tail:?}"
            );
            assert_eq!(
                tail.first().map(String::as_str),
                Some("│"),
                "height {height}: {tail:?}"
            );

            // At the top: the thumb starts flush with the track.
            app.scroll_to_top();
            terminal.draw(|frame| app.render(frame)).unwrap();
            let top = track(&terminal);
            assert_eq!(
                top.first().map(String::as_str),
                Some("┃"),
                "height {height}: {top:?}"
            );
            assert_eq!(
                top.last().map(String::as_str),
                Some("│"),
                "height {height}: {top:?}"
            );
        }
    }

    #[test]
    fn skills_open_as_a_palette_submenu() {
        // The palette lists a single Skills entry, not one row per skill.
        let mut palette = CommandPalette::new(palette_items());
        assert_eq!(palette.items.len(), PALETTE_COMMANDS.len());
        palette.insert_query("skill");
        assert_eq!(
            palette.handle_key(KeyCode::Enter, false),
            CommandPaletteAction::Choose(PaletteCommand::Skills)
        );

        let mut app = App::new();
        app.skills = vec![("deploy".into(), "Deploy things".into())];
        activate_palette_command(&mut app, PaletteCommand::Skills, Vec::new());
        let picker = app.skill_picker.as_ref().expect("skill picker opens");
        // The picker lists the whole slash inventory, built-ins first:
        // `/goal` is invocable from the prompt and so from here too.
        assert_eq!(picker.skills.len(), crate::BUILTIN_SLASH_COMMANDS.len() + 1);
        assert!(picker.skills.iter().any(|(name, _)| name == "goal"));
        assert!(picker.skills.iter().any(|(name, _)| name == "deploy"));
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
            PaletteCommand::Model,
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
            PaletteCommand::Reasoning,
            ilar::model::catalog().iter().collect(),
        );
        assert!(app.command_palette.is_none());
        assert!(app.variant_picker.is_some());

        app.variant_picker = None;
        app.command_palette = Some(CommandPalette::new(palette_items()));
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

        let last = *theme::ThemeId::ALL.last().unwrap();
        assert_eq!(
            picker.handle_key(KeyCode::End, false),
            ThemePickerAction::Preview(last)
        );
        assert_eq!(
            picker.handle_key(KeyCode::Enter, false),
            ThemePickerAction::Choose(last)
        );

        // The list is long enough now that typing has to narrow it, and
        // a query that matches nothing falls back to the whole list
        // rather than leaving nothing to preview.
        let mut picker = ThemePicker::new(theme::ThemeId::Terminal);
        for character in "gruv".chars() {
            picker.handle_key(KeyCode::Char(character), false);
        }
        assert!(
            picker
                .matches()
                .iter()
                .take(2)
                .all(|theme| theme.id().starts_with("gruvbox")),
            "{:?}",
            picker.matches().iter().map(|t| t.id()).collect::<Vec<_>>()
        );
        assert_eq!(picker.selected_theme().id(), "gruvbox-dark");
        for _ in 0..4 {
            picker.handle_key(KeyCode::Backspace, false);
        }
        assert_eq!(picker.matches().len(), theme::ThemeId::ALL.len());

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
    fn pending_messages_are_listed_with_their_fates() {
        let mut app = App::new();
        assert!(app.pending_strip_lines(80).is_empty());

        app.pending_steers = vec!["go left".into(), "no wait, right".into()];
        app.queued_messages = vec!["and then stop".into()];
        let lines = app.pending_strip_lines(80);
        let text = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert_eq!(lines.len(), 3, "{text}");
        // Steers first — they deliver sooner — each with its fate.
        assert!(text.contains("steering"), "{text}");
        assert!(text.contains("go left"), "{text}");
        assert!(text.contains("no wait, right"), "{text}");
        assert!(text.contains("queued"), "{text}");
        assert!(text.contains("and then stop"), "{text}");
        let steer_at = text.find("go left").unwrap();
        let queued_at = text.find("and then stop").unwrap();
        assert!(steer_at < queued_at, "queued shown before steering: {text}");
    }

    #[test]
    fn attaching_an_image_gates_on_vision_and_busy_then_rides_the_next_turn() {
        use crate::{Intent, apply_intent};

        let mut app = App::new();
        // A model without vision refuses, naming the model.
        app.current_model = "zai/glm-4.7".into();
        app.attach_image(ilar::session::ImageContent::png(b"fake png bytes"));
        assert!(app.pending_images.is_empty());
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|notice| notice.text.contains("cannot view images"))
        );

        // Mid-turn attachments would ride steering (text-only): refused.
        app.current_model = "openai/gpt-5.6-sol".into();
        app.busy = true;
        app.attach_image(ilar::session::ImageContent::png(b"fake png bytes"));
        assert!(app.pending_images.is_empty());

        // Idle on a vision model: attached, listed, and drained into
        // the turn request with a marker on the transcript row.
        app.busy = false;
        app.attach_image(ilar::session::ImageContent::png(b"fake png bytes"));
        assert_eq!(app.pending_images.len(), 1);
        let strip = app.pending_strip_lines(80);
        let text = strip[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("attached"), "{text}");

        let sent = apply_intent(&mut app, Intent::StartTurn("look at this".into()), None);
        assert!(matches!(
            sent,
            Some(crate::TurnRequest::New(text, images))
                if text == "look at this" && images.len() == 1
        ));
        assert!(app.pending_images.is_empty());
        assert!(
            matches!(app.lines.last(), Some(Line_::User(text)) if text.contains("[image attached: png")),
            "{:?}",
            app.lines.last()
        );
    }

    #[test]
    fn pasted_image_paths_are_recognized_in_the_common_quoting_styles() {
        use std::path::PathBuf;
        let single = |text: &str| dropped_image_paths(text).map(|paths| paths[0].clone());
        // Plain, with the trailing space most terminals append.
        assert_eq!(
            single("/tmp/shot.png "),
            Some(PathBuf::from("/tmp/shot.png"))
        );
        // Quoted (spaces in the name) and backslash-escaped.
        assert_eq!(
            single("'/tmp/my shot.jpeg'"),
            Some(PathBuf::from("/tmp/my shot.jpeg"))
        );
        assert_eq!(single("\"/tmp/x.gif\""), Some(PathBuf::from("/tmp/x.gif")));
        assert_eq!(
            single("/tmp/my\\ shot.webp"),
            Some(PathBuf::from("/tmp/my shot.webp"))
        );
        // A multi-file drop: several paths in one paste, styles mixed.
        assert_eq!(
            dropped_image_paths("/tmp/a.png '/tmp/b c.jpg' /tmp/d\\ e.webp "),
            Some(vec![
                PathBuf::from("/tmp/a.png"),
                PathBuf::from("/tmp/b c.jpg"),
                PathBuf::from("/tmp/d e.webp"),
            ])
        );
        // One stray token poisons the whole paste back to text.
        assert_eq!(dropped_image_paths("look at /tmp/shot.png"), None);
        assert_eq!(dropped_image_paths("/tmp/a.png /tmp/notes.txt"), None);
        assert_eq!(dropped_image_paths("/tmp/notes.txt"), None);
        assert_eq!(dropped_image_paths("a\nb.png"), None);
        assert_eq!(dropped_image_paths("   "), None);
    }

    #[test]
    fn a_multi_file_drop_attaches_every_image() {
        use crate::{Intent, apply_intent};

        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("one.png");
        let second = dir.path().join("two three.png");
        std::fs::write(&first, ilar::image::encode_png(2, 2, &[1u8; 16]).unwrap()).unwrap();
        std::fs::write(&second, ilar::image::encode_png(2, 2, &[2u8; 16]).unwrap()).unwrap();

        let mut app = App::new();
        app.current_model = "openai/gpt-5.6-sol".into();
        let paste = format!("{} '{}'", first.display(), second.display());
        apply_intent(&mut app, Intent::PasteInput(paste), None);

        assert_eq!(app.pending_images.len(), 2);
        assert!(app.input.is_blank(), "paths must not leak into the input");
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|notice| notice.text.contains("2 images attached")),
            "{:?}",
            app.notice
        );

        // A missing file keeps the whole paste as text.
        let broken = format!("{} /tmp/definitely-missing.png", first.display());
        apply_intent(&mut app, Intent::PasteInput(broken.clone()), None);
        assert_eq!(app.pending_images.len(), 2);
        assert!(app.input.text().contains("definitely-missing"));
    }

    #[test]
    fn the_pending_strip_caps_and_counts_the_rest() {
        let mut app = App::new();
        app.pending_steers = (0..6)
            .map(|index| format!("steer {index}").into())
            .collect();
        let lines = app.pending_strip_lines(80);
        assert_eq!(lines.len(), 5);
        let tail = lines
            .last()
            .unwrap()
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(tail.contains("+2 more"), "{tail}");
    }

    #[test]
    fn pending_steers_show_above_the_input() {
        let backend = ratatui::backend::TestBackend::new(80, 16);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.busy = true;
        app.pending_steers = vec!["use the staging config instead".into()];

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
        assert!(
            screen.contains("use the staging config instead"),
            "{screen}"
        );
        assert!(screen.contains("steering"), "{screen}");
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
        assert!(screen.contains("Switch session"), "{screen}");
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
                "· Thought: Answering",
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

    /// The live row and the restored one must show the same diff for
    /// the same edit. They did not: the live path was handed a copy
    /// bounded at 16 KiB, which past the cap is not JSON at all, so the
    /// diff came out empty and the row showed a wall of raw arguments
    /// — while replay, which reads the input from the log, drew the
    /// diff. The publisher now sends the input whole and the row bounds
    /// it for display, which is exactly what replay does.
    #[test]
    fn a_large_edit_diffs_the_same_live_as_on_replay() {
        // Past the 16 KiB display bound, and still inside what the
        // differ will look at: the gap the live path fell into.
        let filler = (0..300)
            .map(|index| format!("line {index} {}", "x".repeat(50)))
            .collect::<Vec<_>>()
            .join("\n");
        let input = serde_json::json!({
            "path": "src/lib.rs",
            "old_string": format!("{filler}\nbefore"),
            "new_string": format!("{filler}\nafter"),
        });
        assert!(
            ilar::agent::tool_argument_detail("edit", &input).contains("truncated"),
            "the fixture has to be past the display bound to be the case at issue"
        );

        let mut app = App::new();
        app.lines.clear();
        app.push_loop_event(&LoopEvent::ToolStarted {
            id: "edit-1".into(),
            name: "edit".into(),
        });
        app.push_loop_event(&LoopEvent::ToolInputComplete {
            id: "edit-1".into(),
            // What the agent loop now publishes: the whole redacted
            // input, unbounded, for the row to bound itself.
            arguments: serde_json::to_string_pretty(&input).unwrap(),
        });

        let Some(Line_::Tool {
            diff,
            argument_detail,
            ..
        }) = app.lines.last()
        else {
            panic!("the edit row");
        };
        assert_eq!(
            *diff,
            crate::diff::tool_diff_value("edit", &input),
            "live and replay draw the same diff"
        );
        assert!(!diff.is_empty(), "and it is not the empty one");
        assert!(
            argument_detail.contains("truncated"),
            "the text kept for display is still bounded"
        );
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
                agent: "explore".into(),
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
            agent: "explore".into(),
            child_session_id: "child-session".into(),
            event: LoopEvent::TextDelta("Nested answer".into()),
        });
        app.push_subagent_activity(&ilar::subagent::SubagentActivity {
            parent_session_id: String::new(),
            parent_call_id: "task-1".into(),
            agent: "explore".into(),
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
            agent: "explore".into(),
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
    fn hovering_a_clickable_row_underlines_it_and_a_plain_row_stays_bare() {
        let mut app = App::new();
        app.push_loop_event(&LoopEvent::ToolStarted {
            id: "call-1".into(),
            name: "read".into(),
        });
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();

        let row_underlined =
            |terminal: &ratatui::Terminal<ratatui::backend::TestBackend>, area: Rect, row: u16| {
                (area.x..area.right()).any(|column| {
                    let cell = &terminal.backend().buffer()[(column, row)];
                    !cell.symbol().trim().is_empty() && cell.modifier.contains(Modifier::UNDERLINED)
                })
            };
        let area = app.transcript_text_area;
        let clickable = app
            .transcript_hit_targets
            .iter()
            .position(|target| target.is_some())
            .expect("a clickable row") as u16;
        // The greeting is plain text: hovering it must change nothing.
        let plain = app
            .transcript_hit_targets
            .iter()
            .position(|target| target.is_none())
            .expect("a plain row") as u16;

        app.update_hover(area.x + 2, area.y + clickable);
        terminal.draw(|frame| app.render(frame)).unwrap();
        assert!(row_underlined(&terminal, area, area.y + clickable));

        app.update_hover(area.x + 2, area.y + plain);
        terminal.draw(|frame| app.render(frame)).unwrap();
        assert!(!row_underlined(&terminal, area, area.y + plain));
        assert!(!row_underlined(&terminal, area, area.y + clickable));
    }

    /// A trackpad reports a drag for a firm tap, and the old rule —
    /// "no drag event ever arrived" — turned every expand into a coin
    /// flip: press, drift a cell, release, and the row did nothing at
    /// all (or copied a character instead of opening).
    #[test]
    fn a_press_that_drifts_a_cell_is_still_a_click() {
        let drifted = |to_column: u16, to_row: u16| {
            let mut app = App::new();
            app.transcript_text_area = Rect::new(4, 2, 40, 3);
            app.transcript_hit_targets =
                vec![Some(TranscriptHitTarget::ToolGroup("group-1".into())); 3];
            app.transcript_cells = vec![
                vec![RenderedCell::Character('a'); 40],
                vec![RenderedCell::Character('b'); 40],
                vec![RenderedCell::Character('c'); 40],
            ];
            app.begin_transcript_selection(10, 3);
            app.drag_transcript_selection(to_column, to_row);
            let copied = app.finish_transcript_selection(to_column, to_row);
            (app.expanded_tool_groups.contains("group-1"), copied)
        };

        // One cell right, one cell down, and a round trip back to the
        // press: all of them are somebody clicking a disclosure.
        assert_eq!(drifted(11, 3), (true, None));
        assert_eq!(drifted(10, 4), (true, None));
        assert_eq!(drifted(10, 3), (true, None));

        // A real selection still selects and toggles nothing.
        let (toggled, copied) = drifted(30, 4);
        assert!(!toggled);
        assert!(copied.is_some(), "a drag across rows copies");
    }

    #[test]
    fn a_click_toggles_the_row_that_was_pressed_even_if_the_stream_shifts_it() {
        let mut app = App::new();
        app.transcript_text_area = Rect::new(4, 2, 40, 1);
        app.transcript_hit_targets = vec![Some(TranscriptHitTarget::ToolGroup("group-1".into()))];

        app.begin_transcript_selection(5, 2);
        // A newer frame rendered before the release: the pressed row
        // scrolled away and this position no longer names a target.
        app.transcript_hit_targets = vec![None];
        assert_eq!(app.finish_transcript_selection(5, 2), None);

        assert!(app.expanded_tool_groups.contains("group-1"));
    }

    #[test]
    fn a_held_press_pins_the_viewport_against_the_tail() {
        let mut app = App::new();
        app.transcript_text_area = Rect::new(4, 2, 40, 5);
        app.transcript_hit_targets = vec![None; 5];
        app.follow_tail = true;
        app.update_scroll_metrics(20, 5);
        assert_eq!(app.scroll_top, 15);

        app.begin_transcript_selection(5, 3);
        // The stream keeps growing while the button is held: the rows
        // under the cursor must not move.
        app.update_scroll_metrics(30, 5);
        assert_eq!(app.scroll_top, 15);

        // Released: following resumes without having been turned off.
        app.finish_transcript_selection(5, 3);
        assert!(app.follow_tail);
        app.update_scroll_metrics(30, 5);
        assert_eq!(app.scroll_top, 25);
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
    fn aborting_a_turn_stops_the_subagent_spinner() {
        let mut app = App::new();
        app.session_id = "root".into();
        app.push_loop_event(&LoopEvent::ToolStarted {
            id: "call-1".into(),
            name: "task".into(),
        });
        // A subagent reports in: the row now spins on its child's
        // behalf, and the child has a tool of its own running.
        for event in [
            LoopEvent::TurnStarted,
            LoopEvent::ToolStarted {
                id: "child-1".into(),
                name: "grep".into(),
            },
        ] {
            app.push_subagent_activity(&ilar::subagent::SubagentActivity {
                parent_session_id: "root".into(),
                parent_call_id: "call-1".into(),
                agent: "explore".into(),
                child_session_id: "child".into(),
                event,
            });
        }
        assert!(matches!(
            app.lines.last(),
            Some(Line_::Tool { child_running: true, child_lines, .. })
                if child_lines.iter().any(|line| matches!(
                    line,
                    Line_::Tool { state: ToolState::Running, .. }
                ))
        ));

        // The turn is aborted. Dropping the parent's tool futures
        // cancels the child, so its final activity never arrives —
        // nothing else will ever clear these rows.
        app.push_loop_event(&LoopEvent::TurnDone {
            outcome: TurnOutcome::Aborted,
        });

        let Some(Line_::Tool {
            state,
            child_running,
            child_lines,
            ..
        }) = app.lines.last()
        else {
            panic!("{:?}", app.lines);
        };
        assert_eq!(*state, ToolState::Failed);
        assert!(!child_running, "the agent row still claims to be working");
        assert!(
            child_lines.iter().all(|line| !matches!(
                line,
                Line_::Tool {
                    state: ToolState::Running,
                    ..
                }
            )),
            "a child tool row is still running: {child_lines:?}"
        );
    }

    /// The other way a live Task loses its child: the turn errors out
    /// instead of being aborted. The child is just as unreachable, so
    /// the same teardown has to happen.
    #[test]
    fn a_turn_error_stops_the_subagent_spinner() {
        let mut app = App::new();
        app.session_id = "root".into();
        app.push_loop_event(&LoopEvent::ToolStarted {
            id: "call-1".into(),
            name: "task".into(),
        });
        for event in [
            LoopEvent::TurnStarted,
            LoopEvent::ToolStarted {
                id: "child-1".into(),
                name: "grep".into(),
            },
        ] {
            app.push_subagent_activity(&ilar::subagent::SubagentActivity {
                parent_session_id: "root".into(),
                parent_call_id: "call-1".into(),
                agent: "explore".into(),
                child_session_id: "child".into(),
                event,
            });
        }

        app.finish_turn(Err(anyhow::anyhow!("provider hung up")));

        let Some(Line_::Tool {
            state,
            child_running,
            child_lines,
            ..
        }) = app
            .lines
            .iter()
            .find(|line| matches!(line, Line_::Tool { .. }))
        else {
            panic!("{:?}", app.lines);
        };
        assert_eq!(*state, ToolState::Failed);
        assert!(!child_running, "the agent row still claims to be working");
        assert!(
            child_lines.iter().all(|line| !matches!(
                line,
                Line_::Tool {
                    state: ToolState::Running,
                    ..
                }
            )),
            "a child tool row is still running: {child_lines:?}"
        );
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

        // A standalone row has no siblings to align with, so its title
        // follows the name directly — agent and tool alike.
        let agent_row = rendered_text(&tool_line(
            "task",
            &ToolKind::Agent {
                name: "explore".into(),
                model: None,
            },
            "Analyze legacy identity fix",
            ToolState::Succeeded,
            120,
            std::time::Duration::ZERO,
            ToolProgress::None,
            now,
        ));
        assert!(
            agent_row.contains("explore ✓ Analyze legacy identity fix"),
            "{agent_row}"
        );
        let tool_row = rendered_text(&tool_line(
            "bash",
            &ToolKind::Tool,
            "cargo test",
            ToolState::Succeeded,
            120,
            std::time::Duration::ZERO,
            ToolProgress::None,
            now,
        ));
        assert!(tool_row.contains("bash ✓ cargo test"), "{tool_row}");

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

    /// `row_count` always counts the tail padding, so `row_count() > 0`
    /// — which the render pass used to ask before inserting a blank row
    /// above the activity line — is true even on a fresh session. The
    /// spacer only earns its place under something.
    #[test]
    fn an_empty_transcript_gets_no_spacer_above_the_activity_line() {
        let mut app = App::new();
        assert!(app.transcript_cache.is_empty());
        assert!(
            app.transcript_cache.row_count() > 0,
            "the padding row is why an emptiness test cannot use row_count"
        );

        app.busy = true;
        app.set_activity(Activity::Thinking);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let bare = app.content_rows;

        app.push_transcript_line(Line_::System("something happened".into()));
        terminal.draw(|frame| app.render(frame)).unwrap();
        assert!(!app.transcript_cache.is_empty());
        assert_eq!(
            app.content_rows,
            bare + 2,
            "the new row and, now that there is one, its spacer"
        );
    }

    /// One appended token must cost one entry, whatever is behind it.
    /// The cache used to clone and deep-compare the whole model per
    /// delta; now the mutation says where it happened and everything
    /// above that line keeps its rows.
    #[test]
    fn a_streaming_delta_touches_one_entry_of_a_thousand() {
        let mut app = App::new();
        app.lines = (0..1_000)
            .map(|index| Line_::System(format!("row {index}")))
            .collect();
        app.lines.push(Line_::Assistant("stream".into()));
        let now = std::time::Instant::now();
        let render = |app: &mut App| {
            app.transcript_cache.update(
                &app.lines,
                &app.expanded_tool_groups,
                app.transcript_revision,
                40,
                now,
                app.activity_started,
            );
        };
        render(&mut app);
        let rendered = app.transcript_cache.rebuilds;
        assert_eq!(rendered, 1_001, "a cold cache renders everything once");
        app.search_query = "row".into();
        assert_eq!(app.transcript_cache.matching_rows("row").len(), 1_000);
        let searched = app.transcript_cache.searched_rows;

        app.push_loop_event(&LoopEvent::TextDelta("ing".into()));
        render(&mut app);
        let matches = app.transcript_cache.matching_rows("row");

        assert_eq!(
            app.transcript_cache.rebuilds,
            rendered + 1,
            "only the entry the delta landed in re-renders"
        );
        assert!(
            app.transcript_cache.searched_rows - searched < 8,
            "search rescans only the rebuilt rows, not {} of them",
            app.transcript_cache.searched_rows - searched
        );
        assert_eq!(matches.len(), 1_000, "kept matches stay whole");
        assert!(
            app.transcript_cache
                .visible_rows(0, usize::MAX, &[])
                .iter()
                .any(|row| rendered_text(&row.line).contains("streaming")),
            "the delta itself is on screen"
        );
    }

    /// The narrowed rebuild has to be indistinguishable from a cold
    /// one. Drive a stream that pushes, edits, regroups, splits and
    /// prunes, and compare frames against a cache that has seen
    /// nothing — the only honest check on where the marks point.
    ///
    /// Run it several ways: checking every frame proves each mark
    /// alone, checking every third proves they chain across mutations
    /// the cache never rendered, and a run whose width flips mid-stream
    /// proves the reset path. The narrow width matters too — hierarchy
    /// and grouping change shape below 64 columns.
    #[test]
    fn narrowed_rebuilds_match_a_cold_cache_frame_for_frame() {
        for (every, widths) in [
            (1usize, [80u16, 80]),
            (3, [80, 80]),
            (1, [40, 40]),
            (2, [120, 120]),
            (1, [80, 40]),
        ] {
            replay_transcript_against_a_cold_cache(every, widths);
        }
    }

    /// Replays the script, rendering every `every` events at
    /// `widths[0]`, then `widths[1]` once past halfway.
    fn replay_transcript_against_a_cold_cache(every: usize, widths: [u16; 2]) {
        fn snapshot(cache: &TranscriptRenderCache) -> Vec<(String, Option<TranscriptHitTarget>)> {
            cache
                .visible_rows(0, usize::MAX, &[])
                .into_iter()
                .map(|row| (rendered_text(&row.line), row.target))
                .collect()
        }

        let now = std::time::Instant::now();
        let tool = |id: &str| LoopEvent::ToolStarted {
            id: id.into(),
            name: "read".into(),
        };
        let done = |id: &str| LoopEvent::ToolFinished {
            id: id.into(),
            name: "read".into(),
            is_error: false,
            result: "ok".into(),
            child_session_id: None,
        };
        let child = |event: LoopEvent| ilar::subagent::SubagentActivity {
            parent_session_id: String::new(),
            parent_call_id: "task-1".into(),
            agent: "explore".into(),
            child_session_id: "child-session".into(),
            event,
        };

        enum Step {
            Loop(LoopEvent),
            Child(LoopEvent),
            Toggle(TranscriptHitTarget),
            Notify,
            CloseRows,
        }
        let script = vec![
            Step::Loop(LoopEvent::TurnStarted),
            Step::Loop(LoopEvent::ReasoningSummaryDelta(
                "**Planning** the read".into(),
            )),
            Step::Loop(LoopEvent::ReasoningSummaryDelta(" carefully".into())),
            Step::Loop(LoopEvent::ReasoningSummaryCompleted),
            Step::Loop(tool("read-1")),
            Step::Loop(LoopEvent::ToolArguments {
                id: "read-1".into(),
                arguments: "src/main.rs".into(),
            }),
            Step::Loop(tool("read-2")),
            Step::Loop(tool("task-1")),
            // Configuring a subagent splits the run of plain calls.
            Step::Loop(LoopEvent::SubagentConfigured {
                id: "task-1".into(),
                description: "survey the tree".into(),
                agent: "explore".into(),
                model: None,
            }),
            Step::Loop(tool("read-3")),
            Step::Loop(LoopEvent::ToolInputComplete {
                id: "read-3".into(),
                arguments: "{\"path\":\"README.md\"}".into(),
            }),
            Step::Loop(done("read-1")),
            Step::Loop(done("read-2")),
            Step::Loop(LoopEvent::StepComplete {
                stop_reason: "tool_use".into(),
                usage: Default::default(),
            }),
            // In-place growth, repeatedly: the row is edited rather than
            // pushed, so nothing but the mark can reveal it.
            Step::Loop(LoopEvent::TextDelta("Here".into())),
            Step::Loop(LoopEvent::TextDelta(" is".into())),
            Step::Loop(LoopEvent::TextDelta(" the".into())),
            Step::Loop(LoopEvent::TextDelta(" answer".into())),
            Step::Loop(LoopEvent::TextDelta(" at last".into())),
            Step::Loop(tool("read-4")),
            Step::Loop(done("read-3")),
            Step::Loop(done("read-4")),
            Step::Child(LoopEvent::ReasoningSummaryDelta("child reasoning".into())),
            Step::Child(tool("child-read")),
            Step::Child(LoopEvent::TextDelta("child reply".into())),
            Step::Toggle(TranscriptHitTarget::Tool("task-1".into())),
            Step::Child(LoopEvent::TextDelta(" continues".into())),
            Step::Child(LoopEvent::TurnDone {
                outcome: TurnOutcome::Completed,
            }),
            Step::Notify,
            // Left open on purpose: the turn below prunes it out of the
            // middle of the transcript, shifting everything after it.
            Step::Loop(LoopEvent::ThinkingDelta("second thoughts".into())),
            Step::Loop(LoopEvent::Compacted {
                context_tokens: 10,
                summary: "compacted".into(),
            }),
            Step::Loop(LoopEvent::Steered {
                text: "also check the tests".into(),
                images: Vec::new(),
            }),
            Step::Loop(LoopEvent::TurnDone {
                outcome: TurnOutcome::Completed,
            }),
            Step::Toggle(TranscriptHitTarget::ToolGroup("live:0:read-1".into())),
            Step::Loop(tool("read-5")),
            Step::CloseRows,
        ];

        let halfway = script.len() / 2;
        let mut app = App::new();
        app.lines.clear();
        for (step, action) in script.into_iter().enumerate() {
            match action {
                Step::Loop(event) => app.push_loop_event(&event),
                Step::Child(event) => app.push_subagent_activity(&child(event)),
                Step::Toggle(target) => app.toggle_transcript_target(target),
                Step::Notify => app.push_notification("a job", "job finished\nwith detail"),
                Step::CloseRows => app.close_open_rows(),
            }
            if !(step + 1).is_multiple_of(every) {
                continue;
            }
            let width = widths[usize::from(step >= halfway)];
            app.transcript_cache.update(
                &app.lines,
                &app.expanded_tool_groups,
                app.transcript_revision,
                width,
                now,
                app.activity_started,
            );
            let mut cold = TranscriptRenderCache::default();
            cold.update(
                &app.lines,
                &app.expanded_tool_groups,
                app.transcript_revision,
                width,
                now,
                app.activity_started,
            );
            let at = format!("step {step} every {every} width {width}");
            assert_eq!(snapshot(&app.transcript_cache), snapshot(&cold), "{at}");
            assert_eq!(
                app.transcript_cache.matching_rows("read"),
                cold.matching_rows("read"),
                "{at}"
            );
        }
    }

    /// A run of tool calls is one entry, so a call arriving at its edge
    /// joins the run rather than starting a second group beside it —
    /// even though the mark names only the new line.
    #[test]
    fn a_tool_call_appended_next_to_a_group_joins_it() {
        let mut app = App::new();
        app.lines.clear();
        let now = std::time::Instant::now();
        for id in ["read-1", "read-2", "read-3"] {
            app.push_loop_event(&LoopEvent::ToolStarted {
                id: id.into(),
                name: "read".into(),
            });
            app.transcript_cache.update(
                &app.lines,
                &app.expanded_tool_groups,
                app.transcript_revision,
                80,
                now,
                app.activity_started,
            );
        }

        let rendered = app
            .transcript_cache
            .visible_rows(0, usize::MAX, &[])
            .iter()
            .map(|row| rendered_text(&row.line))
            .collect::<Vec<_>>();
        let headers = rendered
            .iter()
            .filter(|row| row.contains("tools "))
            .collect::<Vec<_>>();
        assert_eq!(
            headers.len(),
            1,
            "one group, not one per call: {rendered:?}"
        );
        assert!(headers[0].contains("3 calls"), "{headers:?}");
    }

    /// Narrowing is an optimisation, never a contract: an edit the
    /// cache was not told about still re-renders, because an unmarked
    /// revision bump breaks the chain the marks ride on.
    #[test]
    fn an_unmarked_transcript_edit_still_rerenders() {
        let mut app = App::new();
        app.lines = (0..100)
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

        app.lines[0] = Line_::System("edited behind the cache".into());
        app.transcript_revision = app.transcript_revision.wrapping_add(1);
        app.transcript_cache.update(
            &app.lines,
            &app.expanded_tool_groups,
            app.transcript_revision,
            40,
            now,
            app.activity_started,
        );

        let rows = app.transcript_cache.visible_rows(0, 2, &[]);
        assert!(
            rendered_text(&rows[0].line).contains("edited behind the cache"),
            "{:?}",
            rendered_text(&rows[0].line)
        );
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

    /// Expanding a row moves everything below it and nothing above it.
    /// Marking from zero instead re-parsed the markdown, re-wrapped and
    /// re-highlighted the whole session for one click — which is what
    /// made a long transcript take a visible moment to unfold.
    #[test]
    fn toggling_a_row_rebuilds_only_that_row_and_what_follows_it() {
        let toggles = [
            TranscriptHitTarget::Thought("thought:1".into()),
            TranscriptHitTarget::Tool("call-1".into()),
            TranscriptHitTarget::ToolGroup("live:0:call-1".into()),
        ];
        for target in toggles {
            let mut app = App::new();
            app.lines = (0..500)
                .map(|index| Line_::Assistant(format!("## reply {index}\n\nwith *body* text")))
                .collect();
            app.lines.push(Line_::Thought {
                id: "thought:1".into(),
                text: "considering".into(),
                complete: true,
                expanded: false,
            });
            push_tool_row(&mut app.lines, "call-1", "live:0".into(), "read");
            let now = std::time::Instant::now();
            let refresh = |app: &mut App| {
                app.transcript_cache.update(
                    &app.lines,
                    &app.expanded_tool_groups,
                    app.transcript_revision,
                    60,
                    now,
                    app.activity_started,
                );
            };
            refresh(&mut app);
            let rebuilds = app.transcript_cache.rebuilds;

            app.toggle_transcript_target(target.clone());
            refresh(&mut app);

            let rebuilt = app.transcript_cache.rebuilds - rebuilds;
            assert!(
                rebuilt <= 3,
                "{target:?} rebuilt {rebuilt} entries; only the toggled one and its neighbours can move"
            );
        }
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

        // The cache renders the same lines, then holds blank rows below
        // them so the tail does not sit on the input box.
        assert_eq!(actual[..expected.len()], expected[..]);
        assert!(
            actual[expected.len()..]
                .iter()
                .all(|line| line.spans.is_empty()),
            "{:?}",
            &actual[expected.len()..]
        );
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
    fn the_todo_overlay_shows_what_the_sidebar_had_no_room_for() {
        let backend = ratatui::backend::TestBackend::new(140, 12);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.todos.lock().unwrap().items = (0..12)
            .map(|index| ilar::todo::TodoItem {
                content: format!("task {index}"),
                status: if index == 0 {
                    ilar::todo::Status::InProgress
                } else {
                    ilar::todo::Status::Pending
                },
            })
            .collect();

        let screen = |terminal: &ratatui::Terminal<ratatui::backend::TestBackend>| {
            let buffer = terminal.backend().buffer();
            (0..buffer.area.height)
                .map(|row| {
                    (0..buffer.area.width)
                        .map(|column| buffer[(column, row)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        // The sidebar is a handful of rows tall here, so it hides most.
        terminal.draw(|frame| app.render(frame)).unwrap();
        let sidebar = screen(&terminal);
        assert!(sidebar.contains("hidden"), "{sidebar}");
        assert!(!sidebar.contains("task 11"), "{sidebar}");

        app.todos_visible = true;
        assert_eq!(app.active_modal(), Some(Modal::Todos));
        terminal.draw(|frame| app.render(frame)).unwrap();
        let overlay = screen(&terminal);
        assert!(overlay.contains("▸ task 0"), "{overlay}");
        assert!(overlay.contains("0/12 done"), "{overlay}");
        // Rows the sidebar had no room for.
        assert!(overlay.contains("task 7"), "{overlay}");

        // The tail is a scroll away even on a short terminal.
        app.scroll_active_modal(6);
        terminal.draw(|frame| app.render(frame)).unwrap();
        let scrolled = screen(&terminal);
        assert!(scrolled.contains("task 11"), "{scrolled}");
    }

    #[test]
    fn a_finished_aside_opens_the_modal_and_a_failed_one_is_a_notice() {
        let mut app = App::new();
        app.finish_aside("which port?".into(), Ok(Some("8080.".into())));
        let aside = app.aside.take().expect("modal opened");
        assert_eq!(aside.question, "which port?");
        assert_eq!(aside.answer, "8080.");

        // Cancelled or superseded: silence, not a modal or complaint.
        app.finish_aside("old question".into(), Ok(None));
        assert!(app.aside.is_none());

        app.finish_aside("anything".into(), Err(anyhow::anyhow!("provider melted")));
        assert!(app.aside.is_none());
        let notice = app.notice.as_ref().expect("failure notice");
        assert!(notice.text.contains("aside failed"), "{}", notice.text);
    }

    #[test]
    fn the_palette_opens_the_session_search() {
        let mut app = App::new();

        activate_palette_command(&mut app, PaletteCommand::Session, Vec::new());

        assert!(app.session_search.is_some());
        assert_eq!(app.active_modal(), Some(Modal::SessionSearch));
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
            subtask: false,
        }
    }

    /// A command's `model` becomes a one-turn override, armed for the
    /// spawn block to apply; an unknown model declines the send and
    /// restores the input, like an unknown `/name` does.
    #[test]
    fn a_command_model_override_arms_or_declines() {
        let mut app = App::new();
        app.available_models = vec!["openai/gpt-4.1".into(), "zai/glm-4.7".into()];
        let mut fast = command("fast", "Fast pass", "Do $ARGUMENTS");
        fast.model = Some("zai/glm-4.7".into());
        app.commands = vec![fast];

        let sent = crate::prepare_prompt(&mut app, "/fast the sweep".into());
        assert_eq!(sent.as_deref(), Some("Do the sweep"));
        assert_eq!(
            app.pending_model_override,
            Some((Some("zai/glm-4.7".into()), None))
        );

        // A model outside the available set declines: no override, no
        // send, input restored for editing.
        app.pending_model_override = None;
        let mut bad = command("bad", "Bad model", "Body");
        bad.model = Some("nope/absent".into());
        app.commands = vec![bad];
        let sent = crate::prepare_prompt(&mut app, "/bad x".into());
        assert!(sent.is_none());
        assert!(app.pending_model_override.is_none());
        assert_eq!(app.input.text(), "/bad x");

        // An invalid variant for a known model declines the same way.
        let mut wrong = command("wrong", "Bad variant", "Body");
        wrong.model = Some("zai/glm-4.7".into());
        wrong.variant = Some("no-such-variant".into());
        app.commands = vec![wrong];
        assert!(crate::prepare_prompt(&mut app, "/wrong x".into()).is_none());
        assert!(app.pending_model_override.is_none());

        // A command without overrides arms nothing.
        app.commands = vec![command("plain", "Plain", "Body")];
        assert_eq!(
            crate::prepare_prompt(&mut app, "/plain".into()).as_deref(),
            Some("Body")
        );
        assert!(app.pending_model_override.is_none());
    }

    /// Activity from a UI-spawned subtask carries no parent call id and
    /// can never attach to a Tool row; it must be dropped, not
    /// buffered, or one subtask fills the retry queue for the rest of
    /// the session and evicts activity that could still attach.
    #[test]
    fn orphan_subtask_activity_is_dropped_not_buffered() {
        let mut app = App::new();
        for _ in 0..3 {
            app.push_subagent_activity(&ilar::subagent::SubagentActivity {
                parent_session_id: "root".into(),
                parent_call_id: String::new(),
                agent: "explore".into(),
                child_session_id: "child".into(),
                event: LoopEvent::TextDelta("orphan".into()),
            });
        }
        assert!(
            app.pending_subagent_activity.is_empty(),
            "orphan activity must not occupy the retry buffer"
        );

        // Activity with a real call id that has not rendered yet still
        // buffers, as before.
        app.push_subagent_activity(&ilar::subagent::SubagentActivity {
            parent_session_id: "root".into(),
            parent_call_id: "task-9".into(),
            agent: "explore".into(),
            child_session_id: "child".into(),
            event: LoopEvent::TextDelta("early".into()),
        });
        assert_eq!(app.pending_subagent_activity.len(), 1);
    }

    /// `agent` or `subtask: true` runs the command as a background
    /// subagent: no main-session turn, the request carries the expanded
    /// body and the overrides.
    #[test]
    fn a_subtask_command_becomes_a_task_request_not_a_turn() {
        let mut app = App::new();
        let mut scout = command("scout", "Scout it", "Investigate $ARGUMENTS");
        scout.agent = Some("explore".into());
        scout.model = Some("zai/glm-4.7".into());
        app.commands = vec![scout];

        assert!(crate::prepare_prompt(&mut app, "/scout the crash".into()).is_none());
        let request = app.pending_subtask.take().expect("a task request");
        assert_eq!(request.agent, "explore");
        assert_eq!(request.prompt, "Investigate the crash");
        assert_eq!(request.model.as_deref(), Some("zai/glm-4.7"));
        assert!(!app.busy, "the main session stays idle");

        // subtask: true without an agent defaults to build.
        let mut chore = command("chore", "Chore", "Do the chore");
        chore.subtask = true;
        app.commands = vec![chore];
        assert!(crate::prepare_prompt(&mut app, "/chore".into()).is_none());
        assert_eq!(app.pending_subtask.take().expect("request").agent, "build");
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
            crate::SlashResolution::Prompt(text, _) => {
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
            crate::SlashResolution::Prompt(text, _) if text == "Command body"
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
        assert_eq!(
            inventory
                .iter()
                .find(|(name, _)| name == "review")
                .map(|(_, description)| description.as_str()),
            Some("Command review")
        );
        assert!(inventory.iter().any(|(name, _)| name == "other"));
        // The built-ins lead the list, ahead of anything user-supplied.
        assert_eq!(inventory[0].0, "goal");
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
            crate::SlashResolution::Prompt(text, _) if text == "something"
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

    /// The wiring, not just the decision: an intent has to actually
    /// change the app and hand back a prompt to send. Phase one could
    /// not assert this — every call site could be gutted and the suite
    /// stayed green.
    #[test]
    fn intents_change_the_app_and_yield_the_prompt_to_send() {
        use crate::{Intent, apply_intent};

        let mut app = App::new();
        app.queued_messages = vec!["first".into(), "second".into()];

        // Sending from the queue takes the head and becomes a turn.
        let sent = apply_intent(&mut app, Intent::SendQueued, None);
        assert_eq!(
            sent,
            Some(crate::TurnRequest::New("first".into(), Vec::new()))
        );
        assert_eq!(waiting_texts(&app.queued_messages), vec!["second"]);
        assert!(app.busy, "a started turn marks the app busy");
        assert!(
            matches!(app.lines.last(), Some(Line_::User(text)) if text == "first"),
            "the prompt is echoed into the transcript"
        );

        // An empty queue starts nothing.
        app.queued_messages.clear();
        assert_eq!(apply_intent(&mut app, Intent::SendQueued, None), None);

        // Goal bookkeeping does not start a turn.
        app.goal = Some(("ship it".into(), 3));
        assert_eq!(apply_intent(&mut app, Intent::AdvanceGoal(4), None), None);
        assert_eq!(app.goal.as_ref().map(|(_, round)| *round), Some(4));
        assert_eq!(apply_intent(&mut app, Intent::ClearGoal, None), None);
        assert!(app.goal.is_none());

        // Starting a turn withdraws the retry offer.
        app.retry_available = true;
        app.queued_messages = vec!["again".into()];
        assert!(apply_intent(&mut app, Intent::SendQueued, None).is_some());
        assert!(!app.retry_available);
    }

    #[test]
    fn resume_intent_starts_work_without_replaying_ui_prompt() {
        use crate::{Intent, TurnRequest, apply_intent};

        let mut app = App::new();
        app.retry_available = true;
        let lines_before = app.lines.len();

        let request = apply_intent(&mut app, Intent::ResumeTurn, None);

        assert_eq!(request, Some(TurnRequest::Resume));
        assert_eq!(app.lines.len(), lines_before, "resume added a user line");
        assert!(!app.retry_available);
        assert!(app.turn_committed);
        assert!(app.busy);
    }

    /// End to end over the decision layer: a finished turn with a queued
    /// message must produce the intents that send it, and applying them
    /// must actually send it.
    #[test]
    fn a_finished_turn_drains_the_queue_through_intents() {
        use crate::decide::{LoopState, after_turn};
        use crate::{Intent, apply_intent};

        let mut app = App::new();
        app.queued_messages = vec!["do the next thing".into()];
        let state = LoopState {
            input_blank: true,
            queued: app.queued_messages.len(),
            ..LoopState::default()
        };

        let intents = after_turn(&state, true, None, false, 25);
        assert_eq!(intents, vec![Intent::SendQueued]);

        let mut started = None;
        for intent in intents {
            started = started.or(apply_intent(&mut app, intent, None));
        }
        assert_eq!(
            started,
            Some(crate::TurnRequest::New(
                "do the next thing".into(),
                Vec::new()
            ))
        );
        assert!(app.queued_messages.is_empty());
    }

    /// A queued `/goal` must still arm the goal when it finally sends.
    /// Routing it straight to the model sent the literal text instead —
    /// the old code only expanded on the interactive Enter path.
    #[test]
    fn a_queued_slash_invocation_is_still_expanded_when_it_sends() {
        use crate::{Intent, apply_intent};

        let mut app = App::new();
        app.commands = vec![command("greptile", "Greptile", "Review $ARGUMENTS")];
        app.queued_messages = vec!["/goal ship the parser".into()];

        let sent = apply_intent(&mut app, Intent::SendQueued, None).expect("the goal kickoff");
        assert!(
            app.goal
                .as_ref()
                .is_some_and(|(goal, _)| goal == "ship the parser"),
            "the goal should be armed, not sent as text: {:?}",
            app.goal
        );
        assert!(
            !matches!(&sent, crate::TurnRequest::New(text, _) if text == "/goal ship the parser"),
            "sent verbatim"
        );
        assert!(
            app.lines
                .iter()
                .any(|line| matches!(line, Line_::System(text) if text.contains("goal armed"))),
            "arming should be announced"
        );

        // Same for a queued command.
        app.queued_messages = vec!["/greptile PR 41".into()];
        let sent = apply_intent(&mut app, Intent::SendQueued, None).expect("the command body");
        assert_eq!(
            sent,
            crate::TurnRequest::New("Review PR 41".into(), Vec::new())
        );
    }

    /// A steer reaches the channel while it lives and falls back to the
    /// queue when it does not — the turn ending mid-submit is exactly
    /// when the channel closes, and the message must survive it.
    #[test]
    fn a_steer_reaches_the_channel_or_falls_back_to_the_queue() {
        use crate::{Intent, apply_intent};

        let mut app = App::new();
        let (tx, mut rx) = ilar::agent::steer_channel();
        assert_eq!(
            apply_intent(&mut app, Intent::Steer("go left".into()), Some(&tx)),
            None
        );
        assert_eq!(rx.try_recv().unwrap().text, "go left");
        assert_eq!(waiting_texts(&app.pending_steers), vec!["go left"]);
        assert!(app.queued_messages.is_empty());

        // Receiver gone: the same intent queues instead of vanishing.
        drop(rx);
        apply_intent(&mut app, Intent::Steer("too late".into()), Some(&tx));
        assert_eq!(waiting_texts(&app.queued_messages), vec!["too late"]);

        // No channel at all — a routed notification turn.
        apply_intent(&mut app, Intent::Steer("no channel".into()), None);
        assert_eq!(
            waiting_texts(&app.queued_messages),
            vec!["too late", "no channel"]
        );
        assert_eq!(waiting_texts(&app.pending_steers), vec!["go left"]);
    }

    /// Attached images ride whichever way a mid-turn message goes.
    /// Submitting with an attachment used to be refused outright, which
    /// left the text back in the box and the images pending; now the
    /// prompt hands them over and the message is whole wherever it
    /// waits.
    #[test]
    fn a_mid_turn_message_takes_its_images_with_it() {
        use crate::{Intent, TurnRequest, apply_intent};

        let screenshot = ilar::session::ImageContent::png(b"screenshot");
        let mut app = App::new();
        let (tx, mut rx) = ilar::agent::steer_channel();
        app.pending_images = vec![screenshot.clone()];

        apply_intent(&mut app, Intent::Steer("look at this".into()), Some(&tx));
        let steered = rx.try_recv().expect("the channel took it");
        assert_eq!(steered.images, vec![screenshot.clone()], "sent bare");
        assert!(app.pending_images.is_empty(), "the prompt kept a copy");
        assert_eq!(app.pending_steers[0].images, vec![screenshot.clone()]);

        // Queued the same way — and the queue is where a steer with no
        // live channel lands, so the images must survive that hop too.
        let mut app = App::new();
        app.pending_images = vec![screenshot.clone()];
        apply_intent(&mut app, Intent::Queue("look at this".into()), None);
        assert!(app.pending_images.is_empty());
        assert_eq!(app.queued_messages[0].images, vec![screenshot.clone()]);

        // Sent, it reaches the spawn exactly as a fresh attachment does.
        let sent = apply_intent(&mut app, Intent::SendQueued, None).expect("a turn to start");
        assert_eq!(
            sent,
            TurnRequest::New("look at this".into(), vec![screenshot])
        );
    }

    /// Delivery removes the pending entry the moment the loop reports
    /// it — the "steering" indicator must never outlive the delivery
    /// it announces.
    #[test]
    fn a_delivered_steer_leaves_the_pending_strip() {
        let mut app = App::new();
        app.pending_steers = vec!["go left".into(), "then stop".into()];

        app.push_loop_event(&LoopEvent::Steered {
            text: "go left".into(),
            images: Vec::new(),
        });

        assert_eq!(waiting_texts(&app.pending_steers), vec!["then stop"]);
        assert!(
            app.lines
                .iter()
                .any(|line| matches!(line, Line_::User(text) if text == "go left")),
            "delivered steer missing from the transcript"
        );
        assert_eq!(app.pending_strip_lines(80).len(), 1);

        app.push_loop_event(&LoopEvent::Steered {
            text: "then stop".into(),
            images: Vec::new(),
        });
        assert!(app.pending_steers.is_empty());
        assert!(app.pending_strip_lines(80).is_empty(), "the strip lingered");
    }

    /// A delivered steer's row is a user message like any other: the
    /// words, then one marker per image, so the transcript shows what
    /// the model was actually given.
    #[test]
    fn a_delivered_steer_shows_its_attachment_in_the_transcript() {
        let mut app = App::new();
        let screenshot = ilar::session::ImageContent::png(b"screenshot");
        app.pending_steers = vec![ilar::agent::Steer {
            text: "look at this".into(),
            images: vec![screenshot.clone()],
        }];
        // The strip says what is waiting, attachment included.
        let strip = format!("{:?}", app.pending_strip_lines(80));
        assert!(strip.contains("1 image"), "{strip}");

        app.push_loop_event(&LoopEvent::Steered {
            text: "look at this".into(),
            images: vec![screenshot.clone()],
        });

        let row = app
            .lines
            .iter()
            .find_map(|line| match line {
                Line_::User(text) if text.starts_with("look at this") => Some(text.clone()),
                _ => None,
            })
            .expect("the steer's row");
        assert_eq!(
            row,
            crate::transcript::user_text_with_images("look at this", &[screenshot])
        );
    }

    /// A task result steered into a running turn wears the same
    /// collapsed task row it gets on a fresh turn — never its raw
    /// envelope in a user row.
    #[test]
    fn a_steered_task_notification_wears_the_task_row() {
        let mut app = App::new();
        app.push_loop_event(&LoopEvent::Steered {
            text: "<task-notification>\nTask \"Close installer blockers\" completed (task_id: abc).\n<result>\n(finished with no text)\n</result>\n</task-notification>".into(),
            images: Vec::new(),
        });

        assert!(
            app.lines
                .iter()
                .any(|line| matches!(line, Line_::Task { .. })),
            "{:?}",
            app.lines
        );
        let rendered = format!("{:?}", app.lines);
        assert!(!rendered.contains("<task-notification>"), "{rendered}");
    }

    /// Paste intents land in the surface the decision named.
    #[test]
    fn paste_intents_land_in_their_surfaces() {
        use crate::{Intent, apply_intent};

        let mut app = App::new();
        apply_intent(&mut app, Intent::PasteInput("hello ".into()), None);
        apply_intent(&mut app, Intent::PasteInput("world".into()), None);
        assert_eq!(app.input.text(), "hello world");

        app.push_transcript_line(Line_::System("needle in here".into()));
        // The search cache fills during a render pass.
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        app.open_search();
        apply_intent(&mut app, Intent::PasteSearch(" needle ".into()), None);
        assert_eq!(app.search_query, "needle");
        assert!(!app.search_matches.is_empty(), "the paste searched");
        app.close_search(false);

        app.open_command_palette();
        apply_intent(&mut app, Intent::PastePalette("pending".into()), None);
        let palette = app.command_palette.as_mut().expect("palette open");
        assert_eq!(
            palette.handle_key(KeyCode::Enter, false),
            crate::modals::CommandPaletteAction::Choose(PaletteCommand::Pending),
            "the pasted query should filter the palette"
        );
        // A palette paste with no palette open is dropped, not a panic.
        app.command_palette = None;
        assert_eq!(
            apply_intent(&mut app, Intent::PastePalette("text".into()), None),
            None
        );
    }

    /// The whole path a mid-turn submit takes: decided as a queue,
    /// applied, then sent by the completion's intents. This is the
    /// sequence the loop performs, driven without the loop.
    #[test]
    fn a_message_submitted_mid_turn_queues_and_then_sends() {
        use crate::decide::{self, LoopState, after_turn};
        use crate::{Intent, apply_intent};

        let mut app = App::new();
        let running = LoopState {
            turn_running: true,
            steerable: false,
            input_blank: true,
            ..LoopState::default()
        };
        for intent in decide::submit(&running, true, "next thing".into()) {
            assert!(!matches!(intent, Intent::StartTurn(_)), "mid-turn submit");
            apply_intent(&mut app, intent, None);
        }
        assert_eq!(waiting_texts(&app.queued_messages), vec!["next thing"]);

        let idle = LoopState {
            input_blank: true,
            queued: app.queued_messages.len(),
            ..LoopState::default()
        };
        let mut started = None;
        for intent in after_turn(&idle, true, None, false, 25) {
            started = started.or(apply_intent(&mut app, intent, None));
        }
        assert_eq!(
            started,
            Some(crate::TurnRequest::New("next thing".into(), Vec::new()))
        );
        assert!(app.queued_messages.is_empty());
    }
}
