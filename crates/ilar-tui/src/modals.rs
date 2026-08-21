//! Overlays that take the keyboard: command palette, pickers, help and
//! the pending manager.
//!
//! `Modal` names which one is in front; `App::active_modal` decides, and
//! both the render pass and the key dispatcher derive from that single
//! value so they cannot disagree.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::text::{
    Truncation, format_tokens_compact, fuzzy_score, text_field_view, truncate_display,
};
use crate::theme;
use crate::theme::{ERROR, MUTED};

/// Where the active modal's rows landed this frame, so a click can be
/// mapped back to the item it shows. Rebuilt by every render pass;
/// stale by definition the moment the modal changes, which is why the
/// renderers return it instead of anything storing it long-term.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct ModalHit {
    /// The inner (borderless) area the rows were drawn into.
    pub(crate) area: Rect,
    /// For each drawn row, the index of the item it shows. `None` for
    /// headers, footers and overflow markers.
    pub(crate) rows: Vec<Option<usize>>,
}

impl ModalHit {
    pub(crate) fn item_at(&self, column: u16, row: u16) -> Option<usize> {
        if column < self.area.x
            || column >= self.area.right()
            || row < self.area.y
            || row >= self.area.bottom()
        {
            return None;
        }
        self.rows
            .get((row - self.area.y) as usize)
            .copied()
            .flatten()
    }
}

/// The one definition of which keys move a list selection. Every modal
/// list answers the same keys because they all ask this; before, each
/// carried its own copy of these two arms.
pub(crate) fn nav_delta(code: KeyCode, control: bool) -> Option<isize> {
    match (code, control) {
        (KeyCode::Up, _) | (KeyCode::Char('p'), true) => Some(-1),
        (KeyCode::Down, _) | (KeyCode::Char('n'), true) => Some(1),
        _ => None,
    }
}

/// The overlay that owns the keyboard. Render and key dispatch both
/// derive their precedence from `App::active_modal`, so adding a variant
/// without wiring both is a compile error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Modal {
    Question,
    PendingManager,
    Help,
    ThemePicker,
    SkillPicker,
    SessionPicker,
    ModelPicker,
    VariantPicker,
    Search,
    CommandPalette,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaletteCommand {
    Model,
    Reasoning,
    Theme,
    Session,
    Usage,
    Compact,
    Export,
    Skills,
    Pending,
    Help,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PaletteCommandDefinition {
    id: PaletteCommand,
    section: &'static str,
    pub(crate) label: &'static str,
    shortcut: &'static str,
    search_terms: &'static str,
}

pub(crate) static PALETTE_COMMANDS: &[PaletteCommandDefinition] = &[
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
    PaletteCommandDefinition {
        id: PaletteCommand::Usage,
        section: "General",
        label: "Session usage",
        shortcut: "",
        search_terms: "usage tokens cost dollars spend total",
    },
    PaletteCommandDefinition {
        id: PaletteCommand::Compact,
        section: "General",
        label: "Compact session",
        shortcut: "",
        search_terms: "compact summarize context shrink history",
    },
    PaletteCommandDefinition {
        id: PaletteCommand::Export,
        section: "General",
        label: "Export transcript",
        shortcut: "",
        search_terms: "export markdown save share transcript write file",
    },
    PaletteCommandDefinition {
        id: PaletteCommand::Skills,
        section: "Skills",
        label: "Invoke skill…",
        shortcut: "/",
        search_terms: "skill skills slash command invoke run",
    },
    PaletteCommandDefinition {
        id: PaletteCommand::Pending,
        section: "General",
        label: "Pending…",
        shortcut: "^Q",
        search_terms: "pending queue queued goal background jobs retry manage",
    },
    PaletteCommandDefinition {
        id: PaletteCommand::Help,
        section: "General",
        label: "Help",
        shortcut: "F1",
        search_terms: "help keys keybindings shortcuts bindings",
    },
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PaletteAction {
    Command(PaletteCommand),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PaletteItem {
    action: PaletteAction,
    label: String,
    shortcut: String,
    search_terms: String,
}

/// Palette entries, one per built-in command.
pub(crate) fn palette_items() -> Vec<PaletteItem> {
    PALETTE_COMMANDS
        .iter()
        .map(|command| PaletteItem {
            action: PaletteAction::Command(command.id),
            label: command.label.to_string(),
            shortcut: command.shortcut.to_string(),
            search_terms: format!("{} {}", command.section, command.search_terms),
        })
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CommandPaletteAction {
    Stay,
    Dismiss,
    Choose(PaletteAction),
}

pub(crate) struct CommandPalette {
    query: String,
    selected: usize,
    pub(crate) items: Vec<PaletteItem>,
}

impl CommandPalette {
    pub(crate) fn new(items: Vec<PaletteItem>) -> Self {
        Self {
            query: String::new(),
            selected: 0,
            items,
        }
    }

    fn filtered_commands(&self) -> Vec<&PaletteItem> {
        let query = self.query.to_lowercase();
        let terms = query.split_whitespace().collect::<Vec<_>>();
        self.items
            .iter()
            .filter(|item| {
                let haystack = format!("{} {} {}", item.label, item.shortcut, item.search_terms)
                    .to_lowercase();
                terms.iter().all(|term| haystack.contains(term))
            })
            .collect()
    }

    /// Click-to-select: the index is into the filtered list.
    pub(crate) fn select(&mut self, index: usize) {
        self.selected = index.min(self.filtered_commands().len().saturating_sub(1));
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        let count = self.filtered_commands().len();
        if count == 0 {
            self.selected = 0;
        } else {
            self.selected = (self.selected as isize + delta).rem_euclid(count as isize) as usize;
        }
    }

    pub(crate) fn insert_query(&mut self, text: &str) {
        self.query
            .extend(text.chars().filter(|character| !character.is_control()));
        self.selected = 0;
    }

    pub(crate) fn handle_key(&mut self, code: KeyCode, control: bool) -> CommandPaletteAction {
        if let Some(delta) = nav_delta(code, control) {
            self.move_selection(delta);
            return CommandPaletteAction::Stay;
        }
        match (code, control) {
            (KeyCode::Esc, _) => CommandPaletteAction::Dismiss,
            (KeyCode::Enter, _) => self
                .filtered_commands()
                .get(self.selected)
                .map(|item| CommandPaletteAction::Choose(item.action.clone()))
                .unwrap_or(CommandPaletteAction::Stay),
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
pub(crate) enum PickerAction {
    Stay,
    Dismiss,
    Choose(String),
}

struct HelpBinding {
    keys: &'static str,
    action: &'static str,
    /// Shown instead of `keys` when the terminal cannot report the
    /// chord (Ctrl-M is plain Enter without the kitty protocol).
    portable_keys: Option<&'static str>,
}

struct HelpSection {
    title: &'static str,
    bindings: &'static [HelpBinding],
}

macro_rules! binding {
    ($keys:literal, $action:literal) => {
        HelpBinding {
            keys: $keys,
            action: $action,
            portable_keys: None,
        }
    };
    ($keys:literal, $action:literal, portable = $portable:literal) => {
        HelpBinding {
            keys: $keys,
            action: $action,
            portable_keys: Some($portable),
        }
    };
}

/// Single source for the help overlay. Keep in sync with the key
/// dispatcher in run_app/handle_prompt_key; the help test spot-checks
/// load-bearing entries.
static HELP_SECTIONS: &[HelpSection] = &[
    HelpSection {
        title: "Input",
        bindings: &[
            binding!("Enter", "send message"),
            binding!("Shift-Enter / Ctrl-J", "insert newline"),
            binding!(
                "Esc / Ctrl-C",
                "dismiss overlay · abort turn · clear input (nothing else)"
            ),
            binding!("Ctrl-D", "quit (blank input, nothing open)"),
            binding!("Ctrl-Q", "pending manager: queue, goal, jobs, retry"),
            binding!("Ctrl-R", "retry the last prompt after an error"),
            binding!("Up / Down", "recall prompt history (blank input)"),
            binding!("Ctrl-A / Ctrl-E", "start / end of line"),
            binding!("Ctrl-K / Ctrl-U", "kill to line end / start"),
            binding!("Ctrl-W", "delete previous word"),
            binding!("Alt-B / Alt-F", "move by word"),
        ],
    },
    HelpSection {
        title: "Transcript",
        bindings: &[
            binding!("Ctrl-F", "search transcript"),
            binding!("PgUp / PgDn", "scroll page"),
            binding!("Alt-U / Alt-D", "scroll half page"),
            binding!("Ctrl-Home / Ctrl-End", "jump to top / tail"),
            binding!("Up / Down", "scroll line (while input has text)"),
            binding!("mouse wheel / drag", "scroll · select and copy"),
            binding!("click ▸/▾", "fold or expand tool details"),
        ],
    },
    HelpSection {
        title: "Pickers",
        bindings: &[
            binding!("Ctrl-P", "command palette"),
            binding!("Ctrl-M / F2", "switch model", portable = "F2"),
            binding!("F3", "switch theme"),
            binding!("Ctrl-X, M / T", "leader: models / themes"),
            binding!("↑↓ · Enter · Esc", "navigate · choose · dismiss"),
        ],
    },
    HelpSection {
        title: "Skills",
        bindings: &[
            binding!("/", "autocomplete commands and skills"),
            binding!("/<name> [args]", "invoke a skill directly"),
        ],
    },
    HelpSection {
        title: "Goal mode",
        bindings: &[
            binding!(
                "/goal <description>",
                "work until achieved (evidence-based)"
            ),
            binding!("/goal", "edit the active goal"),
            binding!("/goal abort", "end goal mode"),
        ],
    },
    HelpSection {
        title: "Session",
        bindings: &[
            binding!("palette: Resume session", "switch to another session"),
            binding!("palette: Session usage", "token and cost totals"),
            binding!("ilar --continue", "resume latest session (CLI)"),
        ],
    },
    HelpSection {
        title: "Help",
        bindings: &[
            binding!("F1", "toggle this overlay"),
            binding!("Esc", "close"),
        ],
    },
];

fn help_lines(width: usize, keyboard_enhanced: bool) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for section in HELP_SECTIONS {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.push(Line::styled(
            truncate_display(section.title, width, Truncation::Right),
            theme::title(theme::MARKUP),
        ));
        for binding in section.bindings {
            // Crossterm resolves CR to Enter before the control-character
            // branch, so without the kitty protocol Ctrl-M is literally
            // Enter and would send the draft. Offer only the portable key.
            let keys = match binding.portable_keys {
                Some(portable) if !keyboard_enhanced => portable,
                _ => binding.keys,
            };
            if width < 30 {
                lines.push(Line::styled(
                    truncate_display(
                        &format!("{} {}", keys, binding.action),
                        width.max(1),
                        Truncation::Right,
                    ),
                    Style::default().fg(theme::SECONDARY),
                ));
                continue;
            }
            let padded = format!("  {:<24}", truncate_display(keys, 24, Truncation::Right));
            let action_width = width.saturating_sub(UnicodeWidthStr::width(padded.as_str()) + 1);
            lines.push(Line::from(vec![
                Span::styled(padded, Style::default().fg(theme::SECONDARY)),
                Span::styled(
                    format!(
                        " {}",
                        truncate_display(binding.action, action_width.max(1), Truncation::Right)
                    ),
                    Style::default().fg(theme::PRIMARY),
                ),
            ]));
        }
    }
    lines
}

/// What the pending manager shows, with labels already baked: the
/// renderer only truncates and styles, so it needs nothing from `App`.
pub(crate) struct PendingSnapshot {
    pub(crate) selected: usize,
    /// The selected row is armed for a destructive action.
    pub(crate) armed: bool,
    pub(crate) rows: Vec<String>,
}

pub(crate) fn render_pending_manager(frame: &mut Frame, snapshot: &PendingSnapshot) -> ModalHit {
    let area = centered_rect(frame.area(), 76, 14);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(theme::focus_border())
        .title(Line::styled(" pending ", theme::title(theme::MARKUP)))
        .title_bottom(
            Line::styled(
                " ↑↓ · Enter edit/act · d delete (×2 for goal/jobs) · Esc ",
                Style::default().fg(theme::MUTED),
            )
            .right_aligned(),
        );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return ModalHit::default();
    }
    if snapshot.rows.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "nothing pending — queued messages, the goal, background jobs, and retry offers appear here",
                Style::default().fg(MUTED),
            )),
            inner,
        );
        return ModalHit::default();
    }
    let lines: Vec<Line<'static>> = snapshot
        .rows
        .iter()
        .enumerate()
        .take(inner.height as usize)
        .map(|(index, label)| {
            let armed = snapshot.armed && index == snapshot.selected;
            let marker = if index == snapshot.selected {
                if armed { "✗ " } else { "> " }
            } else {
                "  "
            };
            let text = truncate_display(
                &format!("{marker}{label}"),
                inner.width as usize,
                Truncation::Right,
            );
            let style = if index == snapshot.selected {
                if armed {
                    // Armed deletion is the one place a full bar is the
                    // point; it is still a colour, not inverted video.
                    Style::default().fg(theme::SELECTED_FG).bg(ERROR)
                } else {
                    theme::selected()
                }
            } else {
                Style::default().fg(theme::PRIMARY)
            };
            Line::styled(
                format!("{text:<width$}", width = inner.width as usize),
                style,
            )
        })
        .collect();
    let hit_rows = (0..lines.len()).map(Some).collect();
    frame.render_widget(Paragraph::new(lines), inner);
    ModalHit {
        area: inner,
        rows: hit_rows,
    }
}

pub(crate) fn render_help(frame: &mut Frame, scroll: usize, keyboard_enhanced: bool) {
    let area = centered_rect(frame.area(), 72, 24);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(theme::focus_border())
        .title(Line::styled(" keys ", theme::title(theme::MARKUP)))
        .title_bottom(
            Line::styled(" ↑↓ scroll · Esc close ", Style::default().fg(theme::MUTED))
                .right_aligned(),
        );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let lines = help_lines(inner.width as usize, keyboard_enhanced);
    let start = scroll.min(lines.len().saturating_sub(inner.height as usize));
    let visible: Vec<Line<'static>> = lines
        .into_iter()
        .skip(start)
        .take(inner.height as usize)
        .collect();
    frame.render_widget(Paragraph::new(visible), inner);
}

/// One latent thing the user may want to inspect, edit, or cancel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PendingItem {
    /// Index into the message queue.
    Queued(usize),
    Goal,
    BackgroundJobs,
    Services,
    Retry,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PendingAction {
    Stay,
    Close,
    DeleteQueued(usize),
    EditQueued(usize),
    AbortGoal,
    EditGoal,
    CancelBackground,
    StopServices,
    DismissRetry,
    RetryNow,
}

/// Modal listing all standing state: queued messages, the goal,
/// background jobs, and the retry offer. Destructive actions arm on the
/// first press and fire on the second.
#[derive(Default)]
pub(crate) struct PendingManager {
    pub(crate) selected: usize,
    pub(crate) armed: Option<PendingItem>,
}

pub(crate) struct SkillPicker {
    pub(crate) skills: Vec<(String, String)>,
    selected: usize,
}

impl SkillPicker {
    pub(crate) fn new(skills: Vec<(String, String)>) -> Self {
        Self {
            skills,
            selected: 0,
        }
    }

    /// Click-to-select: the index comes from the frame's hit map.
    pub(crate) fn select(&mut self, index: usize) {
        self.selected = index.min(self.skills.len().saturating_sub(1));
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        let count = self.skills.len();
        if count == 0 {
            self.selected = 0;
        } else {
            self.selected = (self.selected as isize + delta).rem_euclid(count as isize) as usize;
        }
    }

    pub(crate) fn handle_key(&mut self, code: KeyCode, control: bool) -> PickerAction {
        if let Some(delta) = nav_delta(code, control) {
            self.move_selection(delta);
            return PickerAction::Stay;
        }
        match (code, control) {
            (KeyCode::Esc, _) => PickerAction::Dismiss,
            (KeyCode::Enter, _) => self
                .skills
                .get(self.selected)
                .map(|(name, _)| PickerAction::Choose(name.clone()))
                .unwrap_or(PickerAction::Dismiss),
            _ => PickerAction::Stay,
        }
    }
}

pub(crate) fn render_skill_picker(frame: &mut Frame, picker: &SkillPicker) -> ModalHit {
    let area = centered_rect(frame.area(), 72, 14);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(theme::focus_border())
        .title(Line::styled(" skills ", theme::title(theme::MARKUP)))
        .title_bottom(
            Line::styled(
                " ↑↓ select · Enter insert · Esc cancel ",
                Style::default().fg(theme::MUTED),
            )
            .right_aligned(),
        );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return ModalHit::default();
    }
    let selected = picker.selected.min(picker.skills.len().saturating_sub(1));
    let row_count = inner.height as usize;
    let start = selected
        .saturating_add(1)
        .saturating_sub(row_count)
        .min(picker.skills.len().saturating_sub(row_count));
    let mut lines = Vec::new();
    for (index, (name, description)) in picker.skills.iter().enumerate().skip(start).take(row_count)
    {
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
        lines.push(Line::styled(
            format!("{text:<width$}", width = inner.width as usize),
            style,
        ));
    }
    let rows = (start..start.saturating_add(lines.len()))
        .map(Some)
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
    ModalHit { area: inner, rows }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SessionPickerAction {
    Stay,
    Dismiss,
    Resume(String),
    Delete(String),
    Fork(String),
}

pub(crate) struct SessionPicker {
    pub(crate) sessions: Vec<ilar::session::SessionSummary>,
    query: String,
    pub(crate) selected: usize,
    /// Session id armed for deletion; the next Ctrl-D confirms.
    pending_delete: Option<String>,
}

impl SessionPicker {
    pub(crate) fn new(sessions: Vec<ilar::session::SessionSummary>) -> Self {
        Self {
            sessions,
            query: String::new(),
            selected: 0,
            pending_delete: None,
        }
    }

    /// Sessions matching the query, best fuzzy score first (stable, so
    /// equal scores keep recency order).
    fn filtered(&self) -> Vec<&ilar::session::SessionSummary> {
        let mut scored: Vec<(i64, &ilar::session::SessionSummary)> = self
            .sessions
            .iter()
            .filter_map(|session| {
                let haystack = format!("{} {}", session.title.as_deref().unwrap_or(""), session.id);
                fuzzy_score(&self.query, &haystack).map(|score| (score, session))
            })
            .collect();
        scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        scored.into_iter().map(|(_, session)| session).collect()
    }

    fn selected_id(&self) -> Option<String> {
        self.filtered()
            .get(self.selected)
            .map(|session| session.id.clone())
    }

    /// Click-to-select. Disarms a pending delete, like any other
    /// selection move.
    pub(crate) fn select(&mut self, index: usize) {
        self.pending_delete = None;
        self.selected = index.min(self.filtered().len().saturating_sub(1));
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        self.pending_delete = None;
        let count = self.filtered().len();
        if count == 0 {
            self.selected = 0;
        } else {
            self.selected = (self.selected as isize + delta).rem_euclid(count as isize) as usize;
        }
    }

    pub(crate) fn handle_key(&mut self, code: KeyCode, control: bool) -> SessionPickerAction {
        if let Some(delta) = nav_delta(code, control) {
            self.move_selection(delta);
            return SessionPickerAction::Stay;
        }
        match (code, control) {
            (KeyCode::Esc, _) => SessionPickerAction::Dismiss,
            (KeyCode::Enter, _) => self
                .selected_id()
                .map(SessionPickerAction::Resume)
                .unwrap_or(SessionPickerAction::Dismiss),
            (KeyCode::Char('d'), true) => match (self.selected_id(), self.pending_delete.take()) {
                (Some(id), Some(pending)) if pending == id => SessionPickerAction::Delete(id),
                (Some(id), _) => {
                    self.pending_delete = Some(id);
                    SessionPickerAction::Stay
                }
                (None, _) => SessionPickerAction::Stay,
            },
            (KeyCode::Char('y'), true) => self
                .selected_id()
                .map(SessionPickerAction::Fork)
                .unwrap_or(SessionPickerAction::Stay),
            (KeyCode::Backspace, _) => {
                self.query.pop();
                self.selected = 0;
                self.pending_delete = None;
                SessionPickerAction::Stay
            }
            (KeyCode::Char(character), false) if !character.is_control() => {
                self.query.push(character);
                self.selected = 0;
                self.pending_delete = None;
                SessionPickerAction::Stay
            }
            _ => SessionPickerAction::Stay,
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

pub(crate) fn render_session_picker(frame: &mut Frame, picker: &SessionPicker) -> ModalHit {
    let area = centered_rect(frame.area(), 72, 16);
    frame.render_widget(Clear, area);
    let footer = if area.width < 44 {
        " ↵ resume · ^D del · ^Y fork "
    } else {
        " type to filter · ↵ resume · ^D delete ×2 · ^Y fork · Esc "
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
        return ModalHit::default();
    }
    let mut lines = vec![Line::from(vec![
        Span::styled("filter ", Style::default().fg(MUTED)),
        Span::raw(truncate_display(
            &picker.query,
            (inner.width as usize).saturating_sub(8),
            Truncation::Middle,
        )),
    ])];
    let sessions = picker.filtered();
    if sessions.is_empty() {
        lines.push(Line::styled(
            if picker.sessions.is_empty() {
                "no other sessions"
            } else {
                "no matches"
            },
            Style::default().fg(MUTED),
        ));
        frame.render_widget(Paragraph::new(lines), inner);
        return ModalHit::default();
    }
    let now = std::time::SystemTime::now();
    let selected = picker.selected.min(sessions.len() - 1);
    let row_count = (inner.height as usize).saturating_sub(lines.len()).max(1);
    let start = selected
        .saturating_add(1)
        .saturating_sub(row_count)
        .min(sessions.len().saturating_sub(row_count));
    for (index, session) in sessions.iter().enumerate().skip(start).take(row_count) {
        let marker = if index == selected {
            if picker.pending_delete.as_deref() == Some(session.id.as_str()) {
                "✗ "
            } else {
                "> "
            }
        } else {
            "  "
        };
        let age =
            if picker.pending_delete.as_deref() == Some(session.id.as_str()) && index == selected {
                "^D deletes".to_string()
            } else {
                session_age(session.modified, now)
            };
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
    let mut rows = vec![None];
    rows.extend((start..start.saturating_add(lines.len() - 1)).map(Some));
    frame.render_widget(Paragraph::new(lines), inner);
    ModalHit { area: inner, rows }
}

pub(crate) struct ModelPicker {
    models: Vec<&'static ilar::model::ModelInfo>,
    active_model: String,
    query: String,
    pub(crate) selected: usize,
    pub(crate) error: Option<String>,
}

impl ModelPicker {
    pub(crate) fn new(models: Vec<&'static ilar::model::ModelInfo>, active_model: &str) -> Self {
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
    pub(crate) fn set_query(&mut self, query: &str) {
        self.query = query.to_string();
        self.selected = 0;
    }

    #[cfg(test)]
    fn selected_index(&self) -> usize {
        self.selected
    }

    /// Click-to-select: the index is into the filtered list, which is
    /// what the hit map was built from.
    pub(crate) fn select(&mut self, index: usize) {
        self.selected = index.min(self.filtered_models().len().saturating_sub(1));
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
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

    pub(crate) fn handle_key(&mut self, code: KeyCode, control: bool) -> PickerAction {
        if let Some(delta) = nav_delta(code, control) {
            self.move_selection(delta);
            return PickerAction::Stay;
        }
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
pub(crate) enum VariantPickerAction {
    Stay,
    Dismiss,
    Choose(Option<String>),
}

pub(crate) struct VariantPicker {
    pub(crate) model: &'static ilar::model::ModelInfo,
    active_variant: Option<String>,
    selected: usize,
    pub(crate) error: Option<String>,
}

impl VariantPicker {
    pub(crate) fn new(
        model: &'static ilar::model::ModelInfo,
        active_variant: Option<&str>,
    ) -> Self {
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

    /// Click-to-select. Clears the error like a selection move does.
    pub(crate) fn select(&mut self, index: usize) {
        self.selected = index.min(self.model.variants().len());
        self.error = None;
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
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

    pub(crate) fn handle_key(&mut self, code: KeyCode, control: bool) -> VariantPickerAction {
        if let Some(delta) = nav_delta(code, control) {
            self.move_selection(delta);
            return VariantPickerAction::Stay;
        }
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
pub(crate) enum ThemePickerAction {
    Preview(theme::ThemeId),
    Dismiss,
    Choose(theme::ThemeId),
}

pub(crate) struct ThemePicker {
    pub(crate) active_theme: theme::ThemeId,
    pub(crate) query: String,
    /// Themes matching the query, best first. Never empty while the query
    /// matches nothing — it falls back to the full list, because an empty
    /// picker has nothing to preview.
    matches: Vec<theme::ThemeId>,
    selected: usize,
    pub(crate) error: Option<String>,
}

impl ThemePicker {
    pub(crate) fn new(active_theme: theme::ThemeId) -> Self {
        let selected = theme::ThemeId::ALL
            .iter()
            .position(|candidate| *candidate == active_theme)
            .unwrap_or_default();
        Self {
            active_theme,
            query: String::new(),
            matches: theme::ThemeId::ALL.to_vec(),
            selected,
            error: None,
        }
    }

    pub(crate) fn matches(&self) -> &[theme::ThemeId] {
        &self.matches
    }

    pub(crate) fn selected_index(&self) -> usize {
        self.selected.min(self.matches.len().saturating_sub(1))
    }

    pub(crate) fn selected_theme(&self) -> theme::ThemeId {
        self.matches
            .get(self.selected_index())
            .copied()
            .unwrap_or_default()
    }

    /// Rank by label and id together, so both "Tokyo" and "gruvbox-light"
    /// find their theme. The active theme stays selected if it survives.
    fn refresh(&mut self) -> ThemePickerAction {
        let previous = self.selected_theme();
        let mut ranked: Vec<(i64, theme::ThemeId)> = theme::ThemeId::ALL
            .into_iter()
            .filter_map(|candidate| {
                let haystack = format!("{} {}", candidate.label(), candidate.id());
                fuzzy_score(&self.query, &haystack).map(|score| (score, candidate))
            })
            .collect();
        ranked.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        self.matches = if ranked.is_empty() {
            theme::ThemeId::ALL.to_vec()
        } else {
            ranked.into_iter().map(|(_, theme)| theme).collect()
        };
        self.selected = self
            .matches
            .iter()
            .position(|candidate| *candidate == previous)
            .unwrap_or(0);
        self.error = None;
        ThemePickerAction::Preview(self.selected_theme())
    }

    pub(crate) fn select(&mut self, selected: usize) -> ThemePickerAction {
        self.selected = selected.min(self.matches.len().saturating_sub(1));
        self.error = None;
        ThemePickerAction::Preview(self.selected_theme())
    }

    pub(crate) fn move_selection(&mut self, delta: isize) -> ThemePickerAction {
        let count = self.matches.len().max(1) as isize;
        self.selected = (self.selected_index() as isize + delta).rem_euclid(count) as usize;
        self.error = None;
        ThemePickerAction::Preview(self.selected_theme())
    }

    pub(crate) fn handle_key(&mut self, code: KeyCode, control: bool) -> ThemePickerAction {
        if let Some(delta) = nav_delta(code, control) {
            return self.move_selection(delta);
        }
        match (code, control) {
            (KeyCode::Esc, _) => ThemePickerAction::Dismiss,
            (KeyCode::Enter, _) => ThemePickerAction::Choose(self.selected_theme()),
            (KeyCode::Home, _) => self.select(0),
            (KeyCode::End, _) => self.select(self.matches.len().saturating_sub(1)),
            (KeyCode::Backspace, _) => {
                self.query.pop();
                self.refresh()
            }
            (KeyCode::Char(character), false) if !character.is_control() => {
                self.query.push(character);
                self.refresh()
            }
            _ => ThemePickerAction::Preview(self.selected_theme()),
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

/// Visible window into the command list: `(start, rows)`. When the list
/// doesn't fit, two rows are reserved for the ↑/↓ overflow markers and
/// the window tracks the selection.
fn palette_window(total: usize, available: usize, selected: usize) -> (usize, usize) {
    if total <= available {
        return (0, total);
    }
    let rows = available.saturating_sub(2).max(1);
    let start = selected
        .saturating_add(1)
        .saturating_sub(rows)
        .min(total.saturating_sub(rows));
    (start, rows)
}

pub(crate) fn render_command_palette(frame: &mut Frame, palette: &CommandPalette) -> ModalHit {
    // Size to the full command list (query + blank + section + rows +
    // borders); centered_rect caps it on short terminals, where explicit
    // overflow markers take over.
    let desired_height = (palette.items.len() as u16).saturating_add(4);
    let area = centered_rect(frame.area(), 72, desired_height);
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
        return ModalHit::default();
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
    let mut rows: Vec<Option<usize>> = Vec::new();
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
        let available = inner.height.saturating_sub(lines.len() as u16) as usize;
        let selected = palette.selected.min(commands.len().saturating_sub(1));
        let (start, row_count) = palette_window(commands.len(), available, selected);
        if start > 0 {
            lines.push(Line::styled(
                format!("  ↑ {start} more"),
                Style::default().fg(MUTED),
            ));
        }
        rows = vec![None; lines.len()];
        rows.extend((start..commands.len().min(start.saturating_add(row_count))).map(Some));
        for (index, command) in commands.iter().enumerate().skip(start).take(row_count) {
            let marker = if index == selected { "> " } else { "  " };
            let shortcut = (inner.width >= 32 && !command.shortcut.is_empty())
                .then_some(command.shortcut.as_str());
            let suffix_width = shortcut
                .map(|shortcut| UnicodeWidthStr::width(shortcut).saturating_add(1))
                .unwrap_or(0);
            let label_width = (inner.width as usize)
                .saturating_sub(UnicodeWidthStr::width(marker))
                .saturating_sub(suffix_width);
            let label = truncate_display(&command.label, label_width, Truncation::Right);
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
        let below = commands.len().saturating_sub(start + row_count);
        if below > 0 {
            lines.push(Line::styled(
                format!("  ↓ {below} more"),
                Style::default().fg(MUTED),
            ));
        }
    }

    frame.render_widget(Paragraph::new(lines), inner);
    let offset = 7usize
        .saturating_add(query_cursor_offset as usize)
        .min(inner.width.saturating_sub(1) as usize) as u16;
    frame.set_cursor_position((inner.x.saturating_add(offset), inner.y));
    ModalHit { area: inner, rows }
}

pub(crate) fn render_variant_picker(frame: &mut Frame, picker: &VariantPicker) -> ModalHit {
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
        return ModalHit::default();
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
    let mut rows: Vec<Option<usize>> = vec![None; lines.len()];

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
        rows.push(Some(index));
        lines.push(Line::styled(text, style));
    }
    frame.render_widget(Paragraph::new(lines), inner);
    ModalHit { area: inner, rows }
}

pub(crate) fn render_theme_picker(frame: &mut Frame, picker: &ThemePicker) -> ModalHit {
    let area = centered_rect(frame.area(), 58, 20);
    frame.render_widget(Clear, area);

    let footer = if area.width < 32 {
        " ↵ save · Esc undo "
    } else if area.width < 48 {
        " Enter save · Esc undo "
    } else {
        " type to filter · ↑↓ preview · Enter save · Esc undo "
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
        return ModalHit::default();
    }

    let choices = picker.matches();
    let selected = picker.selected_index();
    let mut lines = Vec::new();
    if let Some(error) = &picker.error {
        lines.push(Line::styled(
            truncate_display(error, inner.width as usize, Truncation::Right),
            Style::default().fg(ERROR),
        ));
    } else if picker.query.is_empty() {
        lines.push(Line::styled(
            truncate_display(
                picker.selected_theme().description(),
                inner.width as usize,
                Truncation::Right,
            ),
            Style::default().fg(MUTED),
        ));
    } else {
        lines.push(Line::from(vec![
            Span::styled("/", Style::default().fg(theme::MARKUP)),
            Span::styled(
                truncate_display(
                    &picker.query,
                    (inner.width as usize).saturating_sub(1),
                    Truncation::Right,
                ),
                Style::default().fg(theme::PRIMARY),
            ),
        ]));
    }

    let show_sample = inner.height as usize > choices.len() + 1;
    let row_count = inner
        .height
        .saturating_sub(lines.len() as u16)
        .saturating_sub(u16::from(show_sample))
        .max(1) as usize;
    let start = selected
        .saturating_add(1)
        .saturating_sub(row_count)
        .min(choices.len().saturating_sub(row_count));
    let mut rows: Vec<Option<usize>> = vec![None; lines.len()];
    for (index, choice) in choices.iter().enumerate().skip(start).take(row_count) {
        rows.push(Some(index));
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
    ModalHit { area: inner, rows }
}

pub(crate) fn render_model_picker(frame: &mut Frame, picker: &ModelPicker) -> ModalHit {
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
        return ModalHit::default();
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
    let mut rows: Vec<Option<usize>> = vec![None; lines.len()];
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
        rows.extend((start..models.len().min(start.saturating_add(row_count))).map(Some));
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
    ModalHit { area: inner, rows }
}

pub(crate) fn is_command_palette_shortcut(event: &Event) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::tests::rendered_text;

    #[test]
    fn a_hit_map_answers_only_inside_its_area() {
        let hit = ModalHit {
            area: Rect::new(10, 5, 40, 4),
            rows: vec![None, Some(7), Some(8)],
        };
        assert_eq!(hit.item_at(10, 5), None, "header row");
        assert_eq!(hit.item_at(10, 6), Some(7));
        assert_eq!(hit.item_at(49, 7), Some(8), "last column still hits");
        assert_eq!(hit.item_at(9, 6), None, "left of the area");
        assert_eq!(hit.item_at(50, 6), None, "right of the area");
        assert_eq!(hit.item_at(10, 8), None, "a drawn-short row");
        assert_eq!(hit.item_at(10, 4), None, "above");
        assert_eq!(ModalHit::default().item_at(0, 0), None);
    }

    /// Crossterm maps CR to Enter before the control-character branch, so
    /// without the kitty protocol Ctrl-M *is* Enter and would fire off the
    /// draft. Do not advertise it there.
    #[test]
    fn help_only_offers_ctrl_m_when_the_terminal_can_report_it() {
        let rendered = |enhanced| {
            help_lines(80, enhanced)
                .iter()
                .map(rendered_text)
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert!(rendered(true).contains("Ctrl-M"));
        assert!(
            !rendered(false).contains("Ctrl-M"),
            "Ctrl-M is indistinguishable from Enter without keyboard enhancement"
        );
        // F2 is portable and must always be offered.
        assert!(rendered(false).contains("F2"));
    }

    #[test]
    fn palette_window_fits_or_reserves_marker_rows() {
        // Everything fits: no window.
        assert_eq!(palette_window(6, 10, 0), (0, 6));
        assert_eq!(palette_window(6, 6, 5), (0, 6));
        // Clipped: two rows reserved for markers, window tracks selection.
        assert_eq!(palette_window(6, 5, 0), (0, 3));
        assert_eq!(palette_window(6, 5, 5), (3, 3));
        assert_eq!(palette_window(6, 1, 3), (3, 1));
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
        let mut palette = CommandPalette::new(palette_items());

        assert_eq!(palette.filtered_commands().len(), PALETTE_COMMANDS.len());
        assert_eq!(
            palette.handle_key(KeyCode::Enter, false),
            CommandPaletteAction::Choose(PaletteAction::Command(PaletteCommand::Model))
        );

        palette.insert_query("sessio");
        assert_eq!(
            palette.handle_key(KeyCode::Enter, false),
            CommandPaletteAction::Choose(PaletteAction::Command(PaletteCommand::Session))
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

        let mut palette = CommandPalette::new(palette_items());
        palette.insert_query("model 🚀\n");
        assert_eq!(palette.query, "model 🚀");
        palette.handle_key(KeyCode::Backspace, false);
        assert_eq!(palette.query, "model ");

        let mut palette = CommandPalette::new(palette_items());
        palette.insert_query("theme");
        assert_eq!(palette.filtered_commands().len(), 1);
        assert_eq!(
            palette.handle_key(KeyCode::Enter, false),
            CommandPaletteAction::Choose(PaletteAction::Command(PaletteCommand::Theme))
        );
    }

    #[test]
    fn help_overlay_lists_load_bearing_bindings() {
        let text = help_lines(80, true)
            .iter()
            .map(rendered_text)
            .collect::<Vec<_>>()
            .join("\n");
        for needle in [
            "Ctrl-P",
            "F2",
            "F3",
            "F1",
            "Enter",
            "Shift-Enter",
            "PgUp",
            "Ctrl-U",
            // The exit moved off Ctrl-C; help is where a user looks for it.
            "Ctrl-D",
            "Resume session",
            "history",
            "--continue",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in help:\n{text}");
        }
        // Tiny widths must not panic and must stay within bounds.
        for width in 0..=12 {
            for line in help_lines(width, true) {
                assert!(line.width() <= width.max(1) + 1, "width {width}");
            }
        }
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
        assert_eq!(
            picker.handle_key(KeyCode::Down, false),
            SessionPickerAction::Stay
        );
        assert_eq!(
            picker.handle_key(KeyCode::Enter, false),
            SessionPickerAction::Resume("older".into())
        );
        picker.move_selection(1); // wraps back to the first entry
        assert_eq!(
            picker.handle_key(KeyCode::Enter, false),
            SessionPickerAction::Resume("recent".into())
        );
        assert_eq!(
            picker.handle_key(KeyCode::Esc, false),
            SessionPickerAction::Dismiss
        );

        let mut empty = SessionPicker::new(Vec::new());
        assert_eq!(
            empty.handle_key(KeyCode::Enter, false),
            SessionPickerAction::Dismiss
        );
    }

    #[test]
    fn session_picker_fuzzy_filters_and_arms_deletion() {
        let now = std::time::SystemTime::now();
        let session = |id: &str, title: &str| ilar::session::SessionSummary {
            id: id.into(),
            title: Some(title.into()),
            modified: now,
        };
        let mut picker = SessionPicker::new(vec![
            session("aaa", "fix websearch fallback"),
            session("bbb", "voxel pagoda benchmark"),
            session("ccc", "readline editing chords"),
        ]);
        // fzf-style: subsequence, not substring.
        for character in "vxl".chars() {
            picker.handle_key(KeyCode::Char(character), false);
        }
        assert_eq!(
            picker.handle_key(KeyCode::Enter, false),
            SessionPickerAction::Resume("bbb".into())
        );
        // Backspace edits the query; no match reported gracefully.
        for character in "zzz".chars() {
            picker.handle_key(KeyCode::Char(character), false);
        }
        assert!(picker.filtered().is_empty());
        for _ in 0..6 {
            picker.handle_key(KeyCode::Backspace, false);
        }
        assert_eq!(picker.filtered().len(), 3);

        // Delete requires a confirming second Ctrl-D on the same entry.
        assert_eq!(
            picker.handle_key(KeyCode::Char('d'), true),
            SessionPickerAction::Stay
        );
        assert_eq!(
            picker.handle_key(KeyCode::Char('d'), true),
            SessionPickerAction::Delete("aaa".into())
        );
        // Moving the selection disarms a pending delete.
        picker.handle_key(KeyCode::Char('d'), true);
        picker.move_selection(1);
        assert_eq!(
            picker.handle_key(KeyCode::Char('d'), true),
            SessionPickerAction::Stay
        );
        // Fork is single-press.
        assert_eq!(
            picker.handle_key(KeyCode::Char('y'), true),
            SessionPickerAction::Fork("bbb".into())
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
}
