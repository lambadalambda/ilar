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

/// The chrome every picker modal shares: clear the backdrop, draw the
/// double border in the focus style with the title and the
/// right-aligned muted footer, and hand back the inner area — or
/// `None` when the terminal left no room, which is every caller's cue
/// to bail with an empty hit map. Sizes, titles, colours and footer
/// breakpoints stay with each picker; only the scaffold lives here.
fn modal_frame(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    title_color: ratatui::style::Color,
    footer: &str,
) -> Option<Rect> {
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(theme::focus_border())
        .title(Line::styled(title.to_string(), theme::title(title_color)))
        .title_bottom(
            Line::styled(footer.to_string(), Style::default().fg(theme::MUTED)).right_aligned(),
        );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    (inner.width > 0 && inner.height > 0).then_some(inner)
}

/// A modal's body under construction: every line is pushed together
/// with the item it shows, so the click map cannot drift from what is
/// drawn. (The session picker once counted its rows post-hoc from the
/// line total — one extra header line away from off-by-one clicks.)
#[derive(Default)]
struct ModalRows {
    lines: Vec<Line<'static>>,
    rows: Vec<Option<usize>>,
}

impl ModalRows {
    /// Append a line and, when it shows a selectable item, its index.
    fn push(&mut self, line: Line<'static>, item: Option<usize>) {
        self.lines.push(line);
        self.rows.push(item);
    }

    fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// Draw the collected lines and hand back where the rows landed.
    fn finish(self, frame: &mut Frame, inner: Rect) -> ModalHit {
        frame.render_widget(Paragraph::new(self.lines), inner);
        ModalHit {
            area: inner,
            rows: self.rows,
        }
    }

    /// Draw the collected lines but map no rows: the empty and
    /// no-match states keep clicks inert.
    fn finish_unmapped(self, frame: &mut Frame, inner: Rect) -> ModalHit {
        frame.render_widget(Paragraph::new(self.lines), inner);
        ModalHit::default()
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

/// The selection cursor every picker embeds. `select` clamps because a
/// click can arrive through a stale hit map; `move_by` wraps around the
/// list because the arrow keys and the wheel rely on it. The list
/// length is passed per call — most pickers select within a filtered
/// view whose length changes under the cursor. Reset hooks (armed
/// state, pending deletes, errors, the theme preview) stay in the
/// pickers.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ListNav {
    pub(crate) selected: usize,
}

impl ListNav {
    /// Clamp to the list: a stale click index lands on the last entry.
    fn select(&mut self, index: usize, len: usize) {
        self.selected = index.min(len.saturating_sub(1));
    }

    /// Wrap around the list; an empty list pins the cursor at 0.
    fn move_by(&mut self, delta: isize, len: usize) {
        if len == 0 {
            self.selected = 0;
        } else {
            self.selected = (self.selected as isize + delta).rem_euclid(len as isize) as usize;
        }
    }

    /// Query edits reset to the top: the best match is the first row.
    fn reset(&mut self) {
        self.selected = 0;
    }
}

/// First visible row of a scrolled list: the window is `visible_rows`
/// tall, keeps the selection inside, and never scrolls past the end.
fn list_window(selected: usize, len: usize, visible_rows: usize) -> usize {
    selected
        .saturating_add(1)
        .saturating_sub(visible_rows)
        .min(len.saturating_sub(visible_rows))
}

/// The one query editor. Backspace removes the last grapheme — the
/// palette and the model picker always did, and the codepoint-popping
/// pickers now match. Typed characters append unless they are control
/// characters (the model picker previously accepted them); Ctrl-chords
/// stay free for the picker's own bindings. Returns true when the key
/// was a query edit, so the caller can reset its selection and disarm
/// whatever it had pending — Backspace on an empty query still counts,
/// preserving the old reset-and-disarm behaviour.
fn edit_query(query: &mut String, code: KeyCode, control: bool) -> bool {
    match (code, control) {
        (KeyCode::Backspace, _) => {
            if let Some((index, _)) = query.grapheme_indices(true).next_back() {
                query.truncate(index);
            }
            true
        }
        (KeyCode::Char(character), false) if !character.is_control() => {
            query.push(character);
            true
        }
        _ => false,
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
    Todos,
    Aside,
    ThemePicker,
    SkillPicker,
    SessionPicker,
    SessionSearch,
    TurnPicker,
    LinkPicker,
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
    Rewind,
    Links,
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
        id: PaletteCommand::Session,
        section: "General",
        label: "Switch session",
        shortcut: "",
        search_terms: "session resume continue switch history recent grep search find content",
    },
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
        id: PaletteCommand::Rewind,
        section: "General",
        label: "Rewind to a turn…",
        shortcut: "",
        search_terms: "rewind undo restore checkpoint fork turn back time travel",
    },
    PaletteCommandDefinition {
        id: PaletteCommand::Links,
        section: "General",
        label: "Open link…",
        shortcut: "^O",
        search_terms: "link url open browser web markdown",
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
    nav: ListNav,
    pub(crate) items: Vec<PaletteItem>,
}

impl CommandPalette {
    pub(crate) fn new(items: Vec<PaletteItem>) -> Self {
        Self {
            query: String::new(),
            nav: ListNav::default(),
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
        self.nav.select(index, self.filtered_commands().len());
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        let count = self.filtered_commands().len();
        self.nav.move_by(delta, count);
    }

    pub(crate) fn insert_query(&mut self, text: &str) {
        let before = self.query.len();
        self.query
            .extend(text.chars().filter(|character| !character.is_control()));
        // A paste that was entirely control characters changed nothing
        // and must not move the selection, matching the keyboard path.
        if self.query.len() != before {
            self.nav.reset();
        }
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
                .get(self.nav.selected)
                .map(|item| CommandPaletteAction::Choose(item.action.clone()))
                .unwrap_or(CommandPaletteAction::Stay),
            (KeyCode::Home, _) => {
                self.nav.reset();
                CommandPaletteAction::Stay
            }
            (KeyCode::End, _) => {
                self.nav.selected = self.filtered_commands().len().saturating_sub(1);
                CommandPaletteAction::Stay
            }
            (KeyCode::Backspace, _) | (KeyCode::Char(_), _) => {
                if edit_query(&mut self.query, code, control) {
                    self.nav.reset();
                }
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
            binding!("Ctrl-R", "resume the failed turn from current context"),
            binding!("Ctrl-V", "attach a clipboard image (vision models)"),
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
            binding!("Ctrl-O", "open a link from the transcript"),
            binding!("Ctrl-T", "show the full todo list"),
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
            binding!("/sessions", "grep every session's content; ↵ resumes"),
            binding!(
                "^G in that search",
                "the classic list (filter, delete, fork)"
            ),
            binding!("/btw <question>", "quick aside; answered, never recorded"),
            binding!("palette: Session usage", "token and cost totals"),
            binding!("/rewind", "pick a turn: Enter ×2 rewinds chat + tree"),
            binding!("^Y in that picker", "fork at the turn instead (keeps both)"),
            binding!("/fork", "fork the whole session under a new id"),
            binding!("", "rewind/fork rebuild the session; services stop"),
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
    let Some(inner) = modal_frame(
        frame,
        area,
        " pending ",
        theme::MARKUP,
        " ↑↓ · Enter edit/act · d delete (×2 for goal/jobs) · Esc ",
    ) else {
        return ModalHit::default();
    };
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
    let mut body = ModalRows::default();
    for (index, label) in snapshot.rows.iter().enumerate().take(inner.height as usize) {
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
        body.push(
            Line::styled(
                format!("{text:<width$}", width = inner.width as usize),
                style,
            ),
            Some(index),
        );
    }
    body.finish(frame, inner)
}

pub(crate) fn render_help(frame: &mut Frame, scroll: usize, keyboard_enhanced: bool) {
    let area = centered_rect(frame.area(), 72, 24);
    let Some(inner) = modal_frame(
        frame,
        area,
        " keys ",
        theme::MARKUP,
        " ↑↓ scroll · Esc close ",
    ) else {
        return;
    };
    let lines = help_lines(inner.width as usize, keyboard_enhanced);
    let start = scroll.min(lines.len().saturating_sub(inner.height as usize));
    let visible: Vec<Line<'static>> = lines
        .into_iter()
        .skip(start)
        .take(inner.height as usize)
        .collect();
    frame.render_widget(Paragraph::new(visible), inner);
}

/// The whole todo list, wrapped for `width`. The sidebar shows what
/// fits; this is where the rest lives.
fn todo_overlay_lines(list: &ilar::todo::TodoList, width: usize) -> Vec<Line<'static>> {
    if list.items.is_empty() {
        return vec![Line::styled(
            "— no todos yet; the model writes them as it plans",
            Style::default().fg(MUTED),
        )];
    }
    list.items
        .iter()
        .flat_map(|item| crate::sidebar::todo_item_lines(item, width as u16))
        .collect()
}

/// Read-only overlay over the full todo list. The model owns the list,
/// so there is nothing to select or activate — only to scroll.
pub(crate) fn render_todos(frame: &mut Frame, list: &ilar::todo::TodoList, scroll: usize) {
    let done = list
        .items
        .iter()
        .filter(|item| item.status == ilar::todo::Status::Completed)
        .count();
    let title = if list.items.is_empty() {
        " todos ".to_string()
    } else {
        format!(" todos · {done}/{} done ", list.items.len())
    };
    let area = centered_rect(frame.area(), 72, 24);
    let Some(inner) = modal_frame(
        frame,
        area,
        &title,
        theme::MARKUP,
        " ↑↓ scroll · Esc close ",
    ) else {
        return;
    };
    let lines = todo_overlay_lines(list, inner.width as usize);
    let start = scroll.min(lines.len().saturating_sub(inner.height as usize));
    let visible = lines
        .into_iter()
        .skip(start)
        .take(inner.height as usize)
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(visible), inner);
}

/// A `/btw` exchange: read it, close it. Neither half is part of the
/// session, so the modal is the only place it ever exists.
pub(crate) struct AsideModal {
    pub(crate) question: String,
    pub(crate) answer: String,
    pub(crate) scroll: usize,
}

pub(crate) fn render_aside(frame: &mut Frame, aside: &AsideModal) {
    let area = centered_rect(frame.area(), 90, 28);
    let Some(inner) = modal_frame(
        frame,
        area,
        " btw ",
        theme::MARKUP,
        " ↑↓ scroll · Esc close ",
    ) else {
        return;
    };
    let width = inner.width as usize;
    let mut lines: Vec<Line> = crate::text::wrap_styled_line(
        Line::styled(format!("> {}", aside.question), Style::default().fg(MUTED)),
        width,
    );
    lines.push(Line::raw(""));
    lines.extend(crate::markdown::render(&aside.answer, width));
    let start = aside
        .scroll
        .min(lines.len().saturating_sub(inner.height as usize));
    let visible = lines
        .into_iter()
        .skip(start)
        .take(inner.height as usize)
        .collect::<Vec<_>>();
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
    nav: ListNav,
}

impl SkillPicker {
    pub(crate) fn new(skills: Vec<(String, String)>) -> Self {
        Self {
            skills,
            nav: ListNav::default(),
        }
    }

    /// Click-to-select: the index comes from the frame's hit map.
    pub(crate) fn select(&mut self, index: usize) {
        self.nav.select(index, self.skills.len());
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        self.nav.move_by(delta, self.skills.len());
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
                .get(self.nav.selected)
                .map(|(name, _)| PickerAction::Choose(name.clone()))
                .unwrap_or(PickerAction::Dismiss),
            _ => PickerAction::Stay,
        }
    }
}

pub(crate) fn render_skill_picker(frame: &mut Frame, picker: &SkillPicker) -> ModalHit {
    let area = centered_rect(frame.area(), 72, 14);
    let Some(inner) = modal_frame(
        frame,
        area,
        " skills ",
        theme::MARKUP,
        " ↑↓ select · Enter insert · Esc cancel ",
    ) else {
        return ModalHit::default();
    };
    let selected = picker
        .nav
        .selected
        .min(picker.skills.len().saturating_sub(1));
    let row_count = inner.height as usize;
    let start = list_window(selected, picker.skills.len(), row_count);
    let mut body = ModalRows::default();
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
        body.push(
            Line::styled(
                format!("{text:<width$}", width = inner.width as usize),
                style,
            ),
            Some(index),
        );
    }
    body.finish(frame, inner)
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SessionPickerAction {
    Stay,
    Dismiss,
    Resume(String),
    Delete(String),
    Fork(String),
    /// Switch to the content search: grep what was *said*, not titles.
    ContentSearch,
}

pub(crate) struct SessionPicker {
    pub(crate) sessions: Vec<ilar::session::SessionSummary>,
    query: String,
    pub(crate) nav: ListNav,
    /// Session id armed for deletion; the next Ctrl-D confirms.
    pending_delete: Option<String>,
}

impl SessionPicker {
    pub(crate) fn new(sessions: Vec<ilar::session::SessionSummary>) -> Self {
        Self {
            sessions,
            query: String::new(),
            nav: ListNav::default(),
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
            .get(self.nav.selected)
            .map(|session| session.id.clone())
    }

    /// Click-to-select. Disarms a pending delete, like any other
    /// selection move.
    pub(crate) fn select(&mut self, index: usize) {
        self.pending_delete = None;
        self.nav.select(index, self.filtered().len());
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        self.pending_delete = None;
        let count = self.filtered().len();
        self.nav.move_by(delta, count);
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
            (KeyCode::Char('g'), true) => SessionPickerAction::ContentSearch,
            (KeyCode::Backspace, _) | (KeyCode::Char(_), _) => {
                if edit_query(&mut self.query, code, control) {
                    self.nav.reset();
                    self.pending_delete = None;
                }
                SessionPickerAction::Stay
            }
            _ => SessionPickerAction::Stay,
        }
    }
}

/// One hit of the cross-session content search, carrying everything
/// both panes show so rendering never goes back to disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SearchRow {
    pub(crate) session_id: String,
    /// Topic, or opening message, or the id when the session has
    /// neither — whatever the listing would call it.
    pub(crate) title: String,
    /// Event index of the hit inside its session, shown as an anchor.
    pub(crate) event: usize,
    pub(crate) excerpt: String,
    /// How long since the session was last written, picker-style.
    pub(crate) age: String,
    /// (speaker label, text, is-the-hit) around the match, in order.
    pub(crate) context: Vec<(String, String, bool)>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SessionSearchAction {
    Stay,
    /// The query changed: the running scan is stale, start a new one.
    Rescan,
    Dismiss,
    Resume(String),
    /// Switch to the classic list picker (title filter, delete, fork).
    ListMode,
}

/// Most rows kept for one query; a scan that delivered this many is
/// told to stop. Past a screenful or two, narrowing the query beats
/// scrolling.
pub(crate) const MAX_SEARCH_ROWS: usize = 200;

/// The two-pane content search over every session: matches on the
/// left, the selected match in its surroundings on the right. The scan
/// itself runs elsewhere and streams rows in; the modal only holds
/// what it is shown.
pub(crate) struct SessionSearch {
    pub(crate) query: String,
    pub(crate) rows: Vec<SearchRow>,
    pub(crate) nav: ListNav,
    /// Stamps scan output: rows arriving from an older query's scan
    /// are dropped instead of mixing into the new list.
    pub(crate) generation: u64,
    pub(crate) scanning: bool,
}

impl SessionSearch {
    pub(crate) fn new() -> Self {
        Self {
            query: String::new(),
            rows: Vec::new(),
            nav: ListNav::default(),
            generation: 0,
            // An empty query lists recent sessions, so a scan starts
            // the moment the modal opens.
            scanning: true,
        }
    }

    pub(crate) fn selected(&self) -> Option<&SearchRow> {
        self.rows.get(self.nav.selected)
    }

    pub(crate) fn select(&mut self, index: usize) {
        self.nav.select(index, self.rows.len());
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        self.nav.move_by(delta, self.rows.len());
    }

    /// Accept a batch from the scanner, unless it answers a query the
    /// user has already typed past.
    pub(crate) fn push_rows(&mut self, generation: u64, rows: Vec<SearchRow>) {
        if generation != self.generation {
            return;
        }
        let room = MAX_SEARCH_ROWS.saturating_sub(self.rows.len());
        self.rows.extend(rows.into_iter().take(room));
    }

    /// The scan for `generation` has no more rows to deliver.
    pub(crate) fn finish_scan(&mut self, generation: u64) {
        if generation == self.generation {
            self.scanning = false;
        }
    }

    pub(crate) fn handle_key(&mut self, code: KeyCode, control: bool) -> SessionSearchAction {
        if let Some(delta) = nav_delta(code, control) {
            self.move_selection(delta);
            return SessionSearchAction::Stay;
        }
        match (code, control) {
            (KeyCode::Esc, _) => SessionSearchAction::Dismiss,
            (KeyCode::Char('g'), true) => SessionSearchAction::ListMode,
            (KeyCode::Enter, _) => self
                .selected()
                .map(|row| SessionSearchAction::Resume(row.session_id.clone()))
                .unwrap_or(SessionSearchAction::Stay),
            (KeyCode::Backspace, _) | (KeyCode::Char(_), _) => {
                if edit_query(&mut self.query, code, control) {
                    self.nav.reset();
                    self.rows.clear();
                    self.generation += 1;
                    // Empty query included: that rescans as the
                    // recent-sessions listing.
                    self.scanning = true;
                    return SessionSearchAction::Rescan;
                }
                SessionSearchAction::Stay
            }
            _ => SessionSearchAction::Stay,
        }
    }
}

pub(crate) struct LinkPicker {
    links: Vec<crate::links::LinkEntry>,
    query: String,
    nav: ListNav,
}

impl LinkPicker {
    pub(crate) fn new(links: Vec<crate::links::LinkEntry>) -> Self {
        Self {
            links,
            query: String::new(),
            nav: ListNav::default(),
        }
    }

    fn filtered(&self) -> Vec<&crate::links::LinkEntry> {
        let mut scored: Vec<(i64, &crate::links::LinkEntry)> = self
            .links
            .iter()
            .filter_map(|link| {
                let haystack = format!("{} {}", link.label, link.url);
                fuzzy_score(&self.query, &haystack).map(|score| (score, link))
            })
            .collect();
        scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        scored.into_iter().map(|(_, link)| link).collect()
    }

    pub(crate) fn select(&mut self, index: usize) {
        self.nav.select(index, self.filtered().len());
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        let count = self.filtered().len();
        self.nav.move_by(delta, count);
    }

    pub(crate) fn handle_key(&mut self, code: KeyCode, control: bool) -> PickerAction {
        if let Some(delta) = nav_delta(code, control) {
            self.move_selection(delta);
            return PickerAction::Stay;
        }
        match (code, control) {
            (KeyCode::Esc, _) => PickerAction::Dismiss,
            (KeyCode::Enter, _) => self
                .filtered()
                .get(self.nav.selected)
                .map(|link| PickerAction::Choose(link.url.clone()))
                .unwrap_or(PickerAction::Dismiss),
            (KeyCode::Backspace, _) | (KeyCode::Char(_), _) => {
                if edit_query(&mut self.query, code, control) {
                    self.nav.reset();
                }
                PickerAction::Stay
            }
            _ => PickerAction::Stay,
        }
    }
}

pub(crate) fn render_link_picker(frame: &mut Frame, picker: &LinkPicker) -> ModalHit {
    let area = centered_rect(frame.area(), 80, 16);
    let Some(inner) = modal_frame(
        frame,
        area,
        " links ",
        theme::MARKUP,
        " type to filter · ↵ open in browser · Esc ",
    ) else {
        return ModalHit::default();
    };
    let mut body = ModalRows::default();
    body.push(
        Line::from(vec![
            Span::styled("filter ", Style::default().fg(MUTED)),
            Span::raw(truncate_display(
                &picker.query,
                (inner.width as usize).saturating_sub(8),
                Truncation::Middle,
            )),
        ]),
        None,
    );
    let links = picker.filtered();
    if links.is_empty() {
        body.push(
            Line::styled(
                if picker.links.is_empty() {
                    "no links in this transcript"
                } else {
                    "no matches"
                },
                Style::default().fg(MUTED),
            ),
            None,
        );
        return body.finish_unmapped(frame, inner);
    }
    let selected = picker.nav.selected.min(links.len() - 1);
    let row_count = (inner.height as usize)
        .saturating_sub(body.line_count())
        .max(1);
    let start = list_window(selected, links.len(), row_count);
    for (index, link) in links.iter().enumerate().skip(start).take(row_count) {
        let marker = if index == selected { "> " } else { "  " };
        let width = inner.width as usize;
        let text = if link.label == link.url {
            truncate_display(&format!("{marker}{}", link.url), width, Truncation::Middle)
        } else {
            let url_budget = width.saturating_sub(
                UnicodeWidthStr::width(marker) + UnicodeWidthStr::width(link.label.as_str()) + 1,
            );
            let url = truncate_display(&link.url, url_budget, Truncation::Middle);
            truncate_display(
                &format!("{marker}{} {url}", link.label),
                width,
                Truncation::Middle,
            )
        };
        let style = if index == selected {
            theme::selected()
        } else {
            Style::default().fg(theme::PRIMARY)
        };
        body.push(Line::styled(format!("{text:<width$}"), style), Some(index));
    }
    body.finish(frame, inner)
}

/// One rewindable turn: a user message in the loaded session, newest
/// first in the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TurnEntry {
    /// Local event index of the `UserMessage` (the rewind/fork cut).
    pub(crate) cut: usize,
    /// The message's event id, verified again under the rewind lease.
    pub(crate) user_id: String,
    /// Whitespace-collapsed message text; truncated at render.
    pub(crate) excerpt: String,
    /// Whether the turn has a tree checkpoint to restore.
    pub(crate) has_tree: bool,
    pub(crate) ts: chrono::DateTime<chrono::Utc>,
}

/// Every user message is a valid cut; the entry records whether the
/// checkpoint right before it makes the rewind restore the tree too.
pub(crate) fn turn_entries(events: &[ilar::session::SessionEvent]) -> Vec<TurnEntry> {
    use ilar::session::SessionEvent;
    let mut entries: Vec<TurnEntry> = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| match event {
            SessionEvent::UserMessage { id, text, ts, .. } => Some(TurnEntry {
                cut: index,
                user_id: id.clone(),
                excerpt: text.split_whitespace().collect::<Vec<_>>().join(" "),
                has_tree: index.checked_sub(1).is_some_and(|previous| {
                    matches!(events[previous], SessionEvent::Checkpoint { .. })
                }),
                ts: *ts,
            }),
            _ => None,
        })
        .collect();
    entries.reverse();
    entries
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TurnPickerAction {
    Stay,
    Dismiss,
    /// Confirmed rewind to this turn (second Enter on the armed row).
    Rewind {
        cut: usize,
        target: String,
        discarded: usize,
    },
    /// Fork the session at this turn; non-destructive, no confirmation.
    Fork {
        cut: usize,
        target: String,
    },
}

pub(crate) struct TurnPicker {
    turns: Vec<TurnEntry>,
    query: String,
    nav: ListNav,
    /// User-message id armed for rewind; the next Enter confirms. Any
    /// selection move or filter edit disarms.
    armed: Option<String>,
}

impl TurnPicker {
    pub(crate) fn new(turns: Vec<TurnEntry>) -> Self {
        Self {
            turns,
            query: String::new(),
            nav: ListNav::default(),
            armed: None,
        }
    }

    fn filtered(&self) -> Vec<&TurnEntry> {
        let mut scored: Vec<(i64, &TurnEntry)> = self
            .turns
            .iter()
            .filter_map(|turn| fuzzy_score(&self.query, &turn.excerpt).map(|score| (score, turn)))
            .collect();
        scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        scored.into_iter().map(|(_, turn)| turn).collect()
    }

    fn selected_turn(&self) -> Option<&TurnEntry> {
        self.filtered().get(self.nav.selected).copied()
    }

    /// Discarded-turn count and tree flag for the armed confirmation.
    fn selected_stakes(&self) -> Option<(usize, bool)> {
        let turn = self.selected_turn()?;
        let discarded = self
            .turns
            .iter()
            .filter(|other| other.cut >= turn.cut)
            .count();
        Some((discarded, turn.has_tree))
    }

    pub(crate) fn select(&mut self, index: usize) {
        self.armed = None;
        self.nav.select(index, self.filtered().len());
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        self.armed = None;
        let count = self.filtered().len();
        self.nav.move_by(delta, count);
    }

    pub(crate) fn handle_key(&mut self, code: KeyCode, control: bool) -> TurnPickerAction {
        if let Some(delta) = nav_delta(code, control) {
            self.move_selection(delta);
            return TurnPickerAction::Stay;
        }
        match (code, control) {
            (KeyCode::Esc, _) => TurnPickerAction::Dismiss,
            (KeyCode::Enter, _) => match self.selected_turn() {
                Some(turn) if self.armed.as_deref() == Some(turn.user_id.as_str()) => {
                    TurnPickerAction::Rewind {
                        cut: turn.cut,
                        target: turn.user_id.clone(),
                        discarded: self.selected_stakes().map_or(0, |(discarded, _)| discarded),
                    }
                }
                Some(turn) => {
                    self.armed = Some(turn.user_id.clone());
                    TurnPickerAction::Stay
                }
                None => TurnPickerAction::Dismiss,
            },
            (KeyCode::Char('y'), true) => self
                .selected_turn()
                .map(|turn| TurnPickerAction::Fork {
                    cut: turn.cut,
                    target: turn.user_id.clone(),
                })
                .unwrap_or(TurnPickerAction::Stay),
            (KeyCode::Backspace, _) | (KeyCode::Char(_), _) => {
                if edit_query(&mut self.query, code, control) {
                    self.nav.reset();
                    self.armed = None;
                }
                TurnPickerAction::Stay
            }
            _ => TurnPickerAction::Stay,
        }
    }
}

pub(crate) fn render_turn_picker(frame: &mut Frame, picker: &TurnPicker) -> ModalHit {
    let area = centered_rect(frame.area(), 72, 16);
    let footer = if area.width < 48 {
        " ↵ rewind ×2 · ^Y fork · Esc "
    } else {
        " type to filter · ↵ rewind (×2 confirms) · ^Y fork here · Esc "
    };
    let Some(inner) = modal_frame(frame, area, " rewind to turn ", theme::MARKUP, footer) else {
        return ModalHit::default();
    };
    let mut body = ModalRows::default();
    body.push(
        Line::from(vec![
            Span::styled("filter ", Style::default().fg(MUTED)),
            Span::raw(truncate_display(
                &picker.query,
                (inner.width as usize).saturating_sub(8),
                Truncation::Middle,
            )),
        ]),
        None,
    );
    let turns = picker.filtered();
    if turns.is_empty() {
        body.push(
            Line::styled(
                if picker.turns.is_empty() {
                    "no turns to rewind to"
                } else {
                    "no matches"
                },
                Style::default().fg(MUTED),
            ),
            None,
        );
        return body.finish_unmapped(frame, inner);
    }
    let now = std::time::SystemTime::now();
    let selected = picker.nav.selected.min(turns.len() - 1);
    let row_count = (inner.height as usize)
        .saturating_sub(body.line_count())
        .max(1);
    let start = list_window(selected, turns.len(), row_count);
    for (index, turn) in turns.iter().enumerate().skip(start).take(row_count) {
        let armed = index == selected && picker.armed.as_deref() == Some(turn.user_id.as_str());
        let marker = if index == selected {
            if armed { "✗ " } else { "> " }
        } else {
            "  "
        };
        let right = if armed {
            match picker.selected_stakes() {
                Some((discarded, true)) => format!("↵ drops {discarded}, restores tree"),
                Some((discarded, false)) => format!("↵ drops {discarded}, chat only"),
                None => String::new(),
            }
        } else {
            let age = session_age(std::time::SystemTime::from(turn.ts), now);
            if turn.has_tree {
                format!("⎇ {age}")
            } else {
                age
            }
        };
        let label_width = (inner.width as usize)
            .saturating_sub(UnicodeWidthStr::width(marker))
            .saturating_sub(UnicodeWidthStr::width(right.as_str()))
            .saturating_sub(1);
        let label = truncate_display(&turn.excerpt, label_width, Truncation::Right);
        let text = format!(
            "{marker}{label:<label_width$} {right}",
            label_width = label_width
        );
        let text = truncate_display(&text, inner.width as usize, Truncation::Right);
        let style = if armed {
            Style::default().fg(theme::SELECTED_FG).bg(theme::ERROR)
        } else if index == selected {
            theme::selected()
        } else {
            Style::default().fg(theme::PRIMARY)
        };
        body.push(
            Line::styled(
                format!("{text:<width$}", width = inner.width as usize),
                style,
            ),
            Some(index),
        );
    }
    body.finish(frame, inner)
}

pub(crate) fn session_age(modified: std::time::SystemTime, now: std::time::SystemTime) -> String {
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
    let footer = if area.width < 44 {
        " ↵ resume · ^D del · ^Y fork · ^G grep "
    } else {
        " type to filter · ↵ resume · ^D delete ×2 · ^Y fork · ^G grep content · Esc "
    };
    let Some(inner) = modal_frame(frame, area, " sessions ", theme::MARKUP, footer) else {
        return ModalHit::default();
    };
    let mut body = ModalRows::default();
    body.push(
        Line::from(vec![
            Span::styled("filter ", Style::default().fg(MUTED)),
            Span::raw(truncate_display(
                &picker.query,
                (inner.width as usize).saturating_sub(8),
                Truncation::Middle,
            )),
        ]),
        None,
    );
    let sessions = picker.filtered();
    if sessions.is_empty() {
        body.push(
            Line::styled(
                if picker.sessions.is_empty() {
                    "no other sessions"
                } else {
                    "no matches"
                },
                Style::default().fg(MUTED),
            ),
            None,
        );
        return body.finish_unmapped(frame, inner);
    }
    let now = std::time::SystemTime::now();
    let selected = picker.nav.selected.min(sessions.len() - 1);
    let row_count = (inner.height as usize)
        .saturating_sub(body.line_count())
        .max(1);
    let start = list_window(selected, sessions.len(), row_count);
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
        body.push(
            Line::styled(
                format!("{text:<width$}", width = inner.width as usize),
                style,
            ),
            Some(index),
        );
    }
    body.finish(frame, inner)
}

/// Spans for `text` with every case-insensitive occurrence of `needle`
/// in the highlight style. Byte-exact against the original text: the
/// scan is per-char because lowercasing can change a string's length.
fn highlighted_spans(
    text: &str,
    needle: &str,
    base: Style,
    highlight: Style,
) -> Vec<Span<'static>> {
    let needle_lower = needle.trim().to_lowercase();
    if needle_lower.is_empty() {
        return vec![Span::styled(text.to_string(), base)];
    }
    let mut spans = Vec::new();
    let mut plain_start = 0;
    let mut index = 0;
    while index < text.len() {
        if text[index..].to_lowercase().starts_with(&needle_lower) {
            let matched_len = text[index..]
                .char_indices()
                .nth(needle.trim().chars().count())
                .map(|(offset, _)| offset)
                .unwrap_or(text.len() - index);
            if plain_start < index {
                spans.push(Span::styled(text[plain_start..index].to_string(), base));
            }
            spans.push(Span::styled(
                text[index..index + matched_len].to_string(),
                highlight,
            ));
            index += matched_len;
            plain_start = index;
        } else {
            index += text[index..].chars().next().map_or(1, char::len_utf8);
        }
    }
    if plain_start < text.len() {
        spans.push(Span::styled(text[plain_start..].to_string(), base));
    }
    spans
}

pub(crate) fn render_session_search(frame: &mut Frame, search: &SessionSearch) -> ModalHit {
    let full = frame.area();
    // A workspace, not a prompt: take nearly the whole terminal.
    let area = centered_rect(
        full,
        full.width.saturating_sub(4).min(160),
        full.height.saturating_sub(2),
    );
    // Two readable panes or one; never two unusable ones.
    let wide = area.width >= 96;
    let list_area = if wide {
        Rect {
            width: (area.width as usize * 45 / 100) as u16,
            ..area
        }
    } else {
        area
    };

    let selected = if search.rows.is_empty() {
        0
    } else {
        search.nav.selected.min(search.rows.len() - 1)
    };

    // The preview pane exists whenever the terminal is wide, selection
    // or not — an empty frame still covers whatever sat underneath the
    // modal, where skipping it let the transcript bleed through.
    if wide {
        let preview_area = Rect {
            x: area.x + list_area.width,
            width: area.width - list_area.width,
            ..area
        };
        let row = search.rows.get(selected);
        let title = row
            .map(|row| {
                format!(
                    " {} ",
                    truncate_display(
                        &row.title,
                        preview_area.width.saturating_sub(4) as usize,
                        Truncation::Right
                    )
                )
            })
            .unwrap_or_else(|| " preview ".into());
        let footer = row
            .map(|row| format!(" event {} · {} ", row.event, row.age))
            .unwrap_or_default();
        if let Some(preview_inner) =
            modal_frame(frame, preview_area, &title, theme::PRIMARY, &footer)
            && let Some(row) = row
        {
            let mut lines: Vec<Line> = Vec::new();
            for (speaker, text, is_hit) in &row.context {
                lines.push(Line::styled(
                    format!("· {speaker}"),
                    Style::default().fg(MUTED),
                ));
                let base = if *is_hit {
                    Style::default().fg(theme::PRIMARY)
                } else {
                    Style::default().fg(MUTED)
                };
                lines.push(Line::from(highlighted_spans(
                    text,
                    if *is_hit { &search.query } else { "" },
                    base,
                    theme::title(theme::MARKUP),
                )));
                lines.push(Line::raw(""));
            }
            frame.render_widget(
                Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }),
                preview_inner,
            );
        }
    }

    let status = if search.scanning {
        format!("{} · scanning…", search.rows.len())
    } else {
        format!("{}", search.rows.len())
    };
    let footer = " type to search · ↵ resume · ^G list · Esc ";
    let Some(inner) = modal_frame(
        frame,
        list_area,
        &format!(" search sessions · {status} "),
        theme::MARKUP,
        footer,
    ) else {
        return ModalHit::default();
    };

    let mut body = ModalRows::default();
    body.push(
        Line::from(vec![
            Span::styled("> ", theme::title(theme::MARKUP)),
            Span::raw(truncate_display(
                &search.query,
                (inner.width as usize).saturating_sub(3),
                Truncation::Middle,
            )),
        ]),
        None,
    );
    if search.rows.is_empty() {
        let hint = if search.scanning {
            "scanning…"
        } else if search.query.trim().is_empty() {
            "no other sessions"
        } else {
            "no matches"
        };
        body.push(Line::styled(hint, Style::default().fg(MUTED)), None);
        return body.finish_unmapped(frame, inner);
    }

    let row_count = (inner.height as usize)
        .saturating_sub(body.line_count())
        .max(1);
    let start = list_window(selected, search.rows.len(), row_count);
    let width = inner.width as usize;
    for (index, row) in search.rows.iter().enumerate().skip(start).take(row_count) {
        let marker = if index == selected { "> " } else { "  " };
        // The title gets a bounded column so the excerpt always shows;
        // the age keeps the right edge.
        let title_width = (width / 3).clamp(8, 28);
        let title = truncate_display(&row.title, title_width, Truncation::Right);
        let lead = format!("{marker}{title}: ");
        let age_width = UnicodeWidthStr::width(row.age.as_str());
        let excerpt_budget = width
            .saturating_sub(UnicodeWidthStr::width(lead.as_str()))
            .saturating_sub(age_width + 1);
        let excerpt = truncate_display(&row.excerpt, excerpt_budget, Truncation::Right);
        let pad = excerpt_budget.saturating_sub(UnicodeWidthStr::width(excerpt.as_str())) + 1;
        let line = if index == selected {
            // The bar owns the row; per-span colours would fight it.
            Line::styled(
                format!("{lead}{excerpt}{}{}", " ".repeat(pad), row.age),
                theme::selected(),
            )
        } else {
            let mut spans = vec![
                Span::raw(marker.to_string()),
                Span::styled(format!("{title}: "), Style::default().fg(theme::MARKUP)),
            ];
            spans.extend(highlighted_spans(
                &excerpt,
                &search.query,
                Style::default().fg(theme::PRIMARY),
                theme::title(theme::MARKUP),
            ));
            spans.push(Span::raw(" ".repeat(pad)));
            spans.push(Span::styled(row.age.clone(), Style::default().fg(MUTED)));
            Line::from(spans)
        };
        body.push(line, Some(index));
    }
    body.finish(frame, inner)
}

pub(crate) struct ModelPicker {
    models: Vec<&'static ilar::model::ModelInfo>,
    active_model: String,
    query: String,
    pub(crate) nav: ListNav,
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
            nav: ListNav { selected },
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
        self.nav.reset();
    }

    #[cfg(test)]
    fn selected_index(&self) -> usize {
        self.nav.selected
    }

    /// Click-to-select: the index is into the filtered list, which is
    /// what the hit map was built from.
    pub(crate) fn select(&mut self, index: usize) {
        self.nav.select(index, self.filtered_models().len());
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        let count = self.filtered_models().len();
        self.nav.move_by(delta, count);
    }

    fn select_boundary(&mut self, end: bool) {
        self.nav.selected = if end {
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
                .get(self.nav.selected)
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
            (KeyCode::Backspace, _) | (KeyCode::Char(_), _) => {
                if edit_query(&mut self.query, code, control) {
                    self.nav.reset();
                    self.error = None;
                }
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
    nav: ListNav,
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
            nav: ListNav { selected },
            error: None,
        }
    }

    /// The list is the variants plus the synthetic "Provider default"
    /// row at index 0.
    fn choice_count(&self) -> usize {
        self.model.variants().len() + 1
    }

    #[cfg(test)]
    fn selected_index(&self) -> usize {
        self.nav.selected
    }

    /// Click-to-select. Clears the error like a selection move does.
    pub(crate) fn select(&mut self, index: usize) {
        self.nav.select(index, self.choice_count());
        self.error = None;
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        self.nav.move_by(delta, self.choice_count());
        self.error = None;
    }

    fn selected_variant(&self) -> Option<String> {
        self.nav
            .selected
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
                self.nav.reset();
                VariantPickerAction::Stay
            }
            (KeyCode::End, _) => {
                self.nav.selected = self.model.variants().len();
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
    nav: ListNav,
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
            nav: ListNav { selected },
            error: None,
        }
    }

    pub(crate) fn matches(&self) -> &[theme::ThemeId] {
        &self.matches
    }

    pub(crate) fn selected_index(&self) -> usize {
        self.nav.selected.min(self.matches.len().saturating_sub(1))
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
        self.nav.selected = self
            .matches
            .iter()
            .position(|candidate| *candidate == previous)
            .unwrap_or(0);
        self.error = None;
        ThemePickerAction::Preview(self.selected_theme())
    }

    pub(crate) fn select(&mut self, selected: usize) -> ThemePickerAction {
        self.nav.select(selected, self.matches.len());
        self.error = None;
        ThemePickerAction::Preview(self.selected_theme())
    }

    pub(crate) fn move_selection(&mut self, delta: isize) -> ThemePickerAction {
        // Re-anchor the cursor first: it is only clamped on read.
        self.nav.selected = self.selected_index();
        self.nav.move_by(delta, self.matches.len());
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
            (KeyCode::Backspace, _) | (KeyCode::Char(_), false) => {
                // The selection is not reset here: refresh() re-anchors
                // it on whatever theme was highlighted.
                if edit_query(&mut self.query, code, control) {
                    self.refresh()
                } else {
                    ThemePickerAction::Preview(self.selected_theme())
                }
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
    let footer = if area.width < 44 {
        " Enter select · Esc close "
    } else {
        " ↑↓ move · Enter select · Esc close "
    };
    let Some(inner) = modal_frame(frame, area, " commands ", theme::PRIMARY, footer) else {
        return ModalHit::default();
    };

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
    let mut body = ModalRows::default();
    body.push(
        Line::from(vec![
            Span::styled("search ", Style::default().fg(MUTED)),
            query,
        ]),
        None,
    );
    let commands = palette.filtered_commands();
    if commands.is_empty() {
        if inner.height > 1 {
            body.push(
                Line::styled(" no matching commands", Style::default().fg(MUTED)),
                None,
            );
        }
    } else {
        if inner.height >= 4 {
            body.push(Line::default(), None);
        }
        let available = inner.height.saturating_sub(body.line_count() as u16) as usize;
        let selected = palette.nav.selected.min(commands.len().saturating_sub(1));
        let (start, row_count) = palette_window(commands.len(), available, selected);
        if start > 0 {
            body.push(
                Line::styled(format!("  ↑ {start} more"), Style::default().fg(MUTED)),
                None,
            );
        }
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
            body.push(Line::styled(text, style), Some(index));
        }
        let below = commands.len().saturating_sub(start + row_count);
        if below > 0 {
            body.push(
                Line::styled(format!("  ↓ {below} more"), Style::default().fg(MUTED)),
                None,
            );
        }
    }

    let hit = body.finish(frame, inner);
    let offset = 7usize
        .saturating_add(query_cursor_offset as usize)
        .min(inner.width.saturating_sub(1) as usize) as u16;
    frame.set_cursor_position((inner.x.saturating_add(offset), inner.y));
    hit
}

pub(crate) fn render_variant_picker(frame: &mut Frame, picker: &VariantPicker) -> ModalHit {
    let area = centered_rect(frame.area(), 54, 10);
    let footer = if area.width < 38 {
        " Enter select · Esc close "
    } else {
        " ↑↓ move · Enter select · Esc close "
    };
    let Some(inner) = modal_frame(frame, area, " reasoning ", theme::REASONING, footer) else {
        return ModalHit::default();
    };

    let mut body = ModalRows::default();
    if let Some(error) = &picker.error {
        body.push(
            Line::styled(
                truncate_display(error, inner.width as usize, Truncation::Right),
                Style::default().fg(ERROR),
            ),
            None,
        );
    } else if inner.height >= 6 {
        body.push(
            Line::styled(
                truncate_display(picker.model.name, inner.width as usize, Truncation::Right),
                Style::default().fg(MUTED),
            ),
            None,
        );
    }

    let row_count = inner.height.saturating_sub(body.line_count() as u16) as usize;
    let choice_count = picker.choice_count();
    let selected = picker.nav.selected.min(choice_count.saturating_sub(1));
    let start = list_window(selected, choice_count, row_count);
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
        body.push(Line::styled(text, style), Some(index));
    }
    body.finish(frame, inner)
}

pub(crate) fn render_theme_picker(frame: &mut Frame, picker: &ThemePicker) -> ModalHit {
    let area = centered_rect(frame.area(), 58, 20);
    let footer = if area.width < 32 {
        " ↵ save · Esc undo "
    } else if area.width < 48 {
        " Enter save · Esc undo "
    } else {
        " type to filter · ↑↓ preview · Enter save · Esc undo "
    };
    let Some(inner) = modal_frame(frame, area, " themes ", theme::MARKUP, footer) else {
        return ModalHit::default();
    };

    let choices = picker.matches();
    let selected = picker.selected_index();
    let mut body = ModalRows::default();
    if let Some(error) = &picker.error {
        body.push(
            Line::styled(
                truncate_display(error, inner.width as usize, Truncation::Right),
                Style::default().fg(ERROR),
            ),
            None,
        );
    } else if picker.query.is_empty() {
        body.push(
            Line::styled(
                truncate_display(
                    picker.selected_theme().description(),
                    inner.width as usize,
                    Truncation::Right,
                ),
                Style::default().fg(MUTED),
            ),
            None,
        );
    } else {
        body.push(
            Line::from(vec![
                Span::styled("/", Style::default().fg(theme::MARKUP)),
                Span::styled(
                    truncate_display(
                        &picker.query,
                        (inner.width as usize).saturating_sub(1),
                        Truncation::Right,
                    ),
                    Style::default().fg(theme::PRIMARY),
                ),
            ]),
            None,
        );
    }

    let show_sample = inner.height as usize > choices.len() + 1;
    let row_count = inner
        .height
        .saturating_sub(body.line_count() as u16)
        .saturating_sub(u16::from(show_sample))
        .max(1) as usize;
    let start = list_window(selected, choices.len(), row_count);
    for (index, choice) in choices.iter().enumerate().skip(start).take(row_count) {
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
        body.push(
            Line::styled(
                text,
                if index == selected {
                    theme::selected()
                } else if active {
                    Style::default().fg(theme::SUCCESS)
                } else {
                    Style::default()
                },
            ),
            Some(index),
        );
    }
    if show_sample {
        body.push(
            Line::from(vec![
                Span::styled("you ", theme::title(theme::USER)),
                Span::styled("ilar ", theme::title(theme::ASSISTANT)),
                Span::styled("thought ", Style::default().fg(theme::REASONING)),
                Span::styled("tool ", Style::default().fg(theme::RUNNING)),
                Span::styled("✓ ", Style::default().fg(theme::SUCCESS)),
                Span::styled("×", Style::default().fg(theme::ERROR)),
            ]),
            None,
        );
    }
    body.finish(frame, inner)
}

pub(crate) fn render_model_picker(frame: &mut Frame, picker: &ModelPicker) -> ModalHit {
    let area = centered_rect(frame.area(), 78, 20);
    let footer = if area.width < 44 {
        " Enter select · Esc close "
    } else {
        " ↑↓ move · Enter select · Esc close "
    };
    let Some(inner) = modal_frame(frame, area, " models ", theme::PRIMARY, footer) else {
        return ModalHit::default();
    };

    let models = picker.filtered_models();
    let count = format!(
        "{}/{}",
        picker.nav.selected.saturating_add(1).min(models.len()),
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
    let search_line = Line::from(vec![
        Span::styled("search ", Style::default().fg(MUTED)),
        query,
        Span::raw(" ".repeat(gap)),
        Span::styled(count, Style::default().fg(MUTED)),
    ]);
    let mut body = ModalRows::default();
    if let Some(error) = &picker.error {
        let error = Line::styled(
            truncate_display(error, inner.width as usize, Truncation::Right),
            Style::default().fg(ERROR),
        );
        // On a squeezed terminal the error takes the search line's row.
        if inner.height >= 3 {
            body.push(search_line, None);
        }
        body.push(error, None);
    } else {
        body.push(search_line, None);
        if inner.height >= 6 {
            body.push(
                Line::styled(
                    truncate_display(
                        &format!("current {}", picker.active_model),
                        inner.width as usize,
                        Truncation::Middle,
                    ),
                    Style::default().fg(MUTED),
                ),
                None,
            );
        }
    }
    let row_count = inner.height.saturating_sub(body.line_count() as u16) as usize;
    if models.is_empty() && row_count > 0 {
        body.push(
            Line::styled(" no matching models", Style::default().fg(MUTED)),
            None,
        );
    } else if row_count > 0 {
        let selected = picker.nav.selected.min(models.len().saturating_sub(1));
        let start = list_window(selected, models.len(), row_count);
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
            body.push(Line::styled(text, style), Some(index));
        }
    }

    let hit = body.finish(frame, inner);
    let offset = 7usize
        .saturating_add(query_cursor_offset as usize)
        .min(inner.width.saturating_sub(1) as usize) as u16;
    frame.set_cursor_position((inner.x.saturating_add(offset), inner.y));
    hit
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
    fn user(id: &str, text: &str) -> ilar::session::SessionEvent {
        ilar::session::SessionEvent::UserMessage {
            id: id.into(),
            text: text.into(),
            images: Vec::new(),
            ts: chrono::Utc::now(),
        }
    }

    fn checkpoint(commit: &str) -> ilar::session::SessionEvent {
        ilar::session::SessionEvent::Checkpoint {
            id: format!("cp-{commit}"),
            commit: commit.into(),
            head: None,
            ts: chrono::Utc::now(),
        }
    }

    fn meta_event() -> ilar::session::SessionEvent {
        ilar::session::SessionEvent::Meta {
            meta: ilar::session::SessionMeta {
                session_id: "s".into(),
                parent_id: None,
                agent: "build".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
            },
            ts: chrono::Utc::now(),
        }
    }

    #[test]
    fn turn_entries_are_newest_first_with_tree_flags_and_cuts() {
        let events = vec![
            meta_event(),
            checkpoint("aaa"),
            user("u1", "  first   question  "),
            user("u2", "a steer"),
            checkpoint("bbb"),
            user("u3", "second question"),
        ];
        let entries = turn_entries(&events);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].cut, 5);
        assert!(entries[0].has_tree);
        assert_eq!(entries[0].excerpt, "second question");
        assert_eq!(entries[1].cut, 3);
        assert!(!entries[1].has_tree, "a steer has no checkpoint before it");
        assert_eq!(entries[2].cut, 2);
        assert!(entries[2].has_tree);
        assert_eq!(entries[2].excerpt, "first question", "whitespace collapses");
        assert_eq!(entries[2].user_id, "u1");
    }

    #[test]
    fn turn_picker_arms_then_rewinds_and_navigation_disarms() {
        let events = vec![meta_event(), user("u1", "first"), user("u2", "second")];
        let mut picker = TurnPicker::new(turn_entries(&events));

        // First Enter arms; second confirms with the newest turn's cut.
        assert_eq!(
            picker.handle_key(KeyCode::Enter, false),
            TurnPickerAction::Stay
        );
        assert_eq!(
            picker.handle_key(KeyCode::Enter, false),
            TurnPickerAction::Rewind {
                cut: 2,
                target: "u2".into(),
                discarded: 1
            }
        );

        // Arm, then move: disarmed, so Enter re-arms instead of firing.
        picker.handle_key(KeyCode::Enter, false);
        picker.handle_key(KeyCode::Down, false);
        assert_eq!(
            picker.handle_key(KeyCode::Enter, false),
            TurnPickerAction::Stay
        );

        // Arm, then filter edit: also disarmed.
        let mut picker = TurnPicker::new(turn_entries(&events));
        picker.handle_key(KeyCode::Enter, false);
        picker.handle_key(KeyCode::Char('f'), false);
        assert_eq!(
            picker.handle_key(KeyCode::Enter, false),
            TurnPickerAction::Stay
        );
    }

    #[test]
    fn turn_picker_fork_fires_immediately_without_confirmation() {
        let events = vec![meta_event(), user("u1", "first"), user("u2", "second")];
        let mut picker = TurnPicker::new(turn_entries(&events));
        assert_eq!(
            picker.handle_key(KeyCode::Char('y'), true),
            TurnPickerAction::Fork {
                cut: 2,
                target: "u2".into()
            }
        );
    }

    #[test]
    fn empty_turn_picker_dismisses_on_enter() {
        let mut picker = TurnPicker::new(Vec::new());
        assert_eq!(
            picker.handle_key(KeyCode::Enter, false),
            TurnPickerAction::Dismiss
        );
    }

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
    fn list_nav_clamps_selection_and_wraps_moves() {
        let mut nav = ListNav::default();
        nav.select(7, 3);
        assert_eq!(nav.selected, 2, "a stale click lands on the last entry");
        nav.select(1, 3);
        assert_eq!(nav.selected, 1);
        nav.move_by(2, 3);
        assert_eq!(nav.selected, 0, "moving past the end wraps to the top");
        nav.move_by(-1, 3);
        assert_eq!(nav.selected, 2, "moving before the start wraps to the end");
        nav.move_by(5, 0);
        assert_eq!(nav.selected, 0, "an empty list pins the cursor");
        nav.select(0, 0);
        assert_eq!(nav.selected, 0);
    }

    #[test]
    fn list_window_tracks_the_selection_without_overshooting() {
        // Everything fits: the window starts at the top.
        assert_eq!(list_window(0, 3, 5), 0);
        assert_eq!(list_window(2, 3, 5), 0);
        // Clipped: the window slides to keep the selection visible.
        assert_eq!(list_window(0, 10, 4), 0);
        assert_eq!(list_window(3, 10, 4), 0);
        assert_eq!(list_window(4, 10, 4), 1);
        assert_eq!(list_window(9, 10, 4), 6);
        // Degenerate sizes must not underflow.
        assert_eq!(list_window(0, 0, 4), 0);
        assert_eq!(list_window(5, 10, 0), 6, "a zero-row window draws nothing");
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

    /// A combining mark arrives as its own key event, so the query can
    /// hold multi-codepoint graphemes. Backspace must remove the whole
    /// grapheme in every query picker — the codepoint-popping pickers used
    /// to strand the base character.
    #[test]
    fn query_backspace_removes_whole_graphemes_in_every_picker() {
        let mut session = SessionPicker::new(Vec::new());
        session.handle_key(KeyCode::Char('e'), false);
        session.handle_key(KeyCode::Char('\u{301}'), false);
        assert_eq!(session.query, "e\u{301}");
        session.handle_key(KeyCode::Backspace, false);
        assert_eq!(session.query, "");

        let mut turn = TurnPicker::new(Vec::new());
        turn.handle_key(KeyCode::Char('e'), false);
        turn.handle_key(KeyCode::Char('\u{301}'), false);
        turn.handle_key(KeyCode::Backspace, false);
        assert_eq!(turn.query, "");

        let mut link = LinkPicker::new(Vec::new());
        link.handle_key(KeyCode::Char('e'), false);
        link.handle_key(KeyCode::Char('\u{301}'), false);
        link.handle_key(KeyCode::Backspace, false);
        assert_eq!(link.query, "");

        let mut theme = ThemePicker::new(theme::ThemeId::ALL[0]);
        theme.handle_key(KeyCode::Char('e'), false);
        theme.handle_key(KeyCode::Char('\u{301}'), false);
        theme.handle_key(KeyCode::Backspace, false);
        assert_eq!(theme.query, "");
    }

    /// The model picker used to append control characters to its query
    /// (and reset the selection while doing it); the shared editor
    /// rejects them everywhere.
    #[test]
    fn model_picker_rejects_control_characters() {
        let models = ilar::model::catalog().iter().take(3).collect();
        let mut picker = ModelPicker::new(models, "missing/model");
        picker.handle_key(KeyCode::Down, false);
        assert_eq!(picker.selected_index(), 1);

        assert_eq!(
            picker.handle_key(KeyCode::Char('\u{7}'), false),
            PickerAction::Stay
        );
        assert_eq!(picker.query, "", "control characters must not filter");
        assert_eq!(
            picker.selected_index(),
            1,
            "a rejected key must not reset the selection"
        );
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
        // Switching sessions leads the list: the most-reached-for entry.
        assert_eq!(
            palette.handle_key(KeyCode::Enter, false),
            CommandPaletteAction::Choose(PaletteAction::Command(PaletteCommand::Session))
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
    fn an_all_control_character_paste_is_a_full_no_op() {
        let mut palette = CommandPalette::new(palette_items());
        palette.move_selection(2);
        let selected_before = palette.nav.selected;

        palette.insert_query("\x1b\x07\n");

        assert_eq!(palette.query, "");
        assert_eq!(palette.nav.selected, selected_before);
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
            "Ctrl-O",
            "/rewind",
            "/sessions",
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
    fn the_todo_overlay_lists_everything_the_sidebar_hid() {
        let list = ilar::todo::TodoList {
            items: (0..12)
                .map(|index| ilar::todo::TodoItem {
                    content: format!("todo number {index}"),
                    status: if index < 3 {
                        ilar::todo::Status::Completed
                    } else if index == 3 {
                        ilar::todo::Status::InProgress
                    } else {
                        ilar::todo::Status::Pending
                    },
                })
                .collect(),
        };

        let text = todo_overlay_lines(&list, 40)
            .iter()
            .map(rendered_text)
            .collect::<Vec<_>>()
            .join("\n");

        for index in 0..12 {
            assert!(
                text.contains(&format!("todo number {index}")),
                "missing {index}:\n{text}"
            );
        }
        assert!(text.contains("✓ todo number 0"), "{text}");
        assert!(text.contains("▸ todo number 3"), "{text}");
        assert!(text.contains("○ todo number 4"), "{text}");
        assert!(!text.contains("hidden"), "{text}");

        // An empty list says so rather than drawing a blank box.
        let empty = todo_overlay_lines(&ilar::todo::TodoList::default(), 40)
            .iter()
            .map(rendered_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(empty.contains("no todos"), "{empty}");
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

    /// Render one modal into a test terminal: the buffer text joined by
    /// newlines, plus the hit map the renderer returned.
    fn draw_modal(
        width: u16,
        height: u16,
        render: impl Fn(&mut Frame) -> ModalHit,
    ) -> (String, ModalHit) {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut hit = ModalHit::default();
        terminal.draw(|frame| hit = render(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let screen = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        (screen, hit)
    }

    fn link(label: &str, url: &str) -> crate::links::LinkEntry {
        crate::links::LinkEntry {
            label: label.into(),
            url: url.into(),
        }
    }

    #[test]
    fn link_picker_navigates_filters_and_chooses() {
        let mut picker = LinkPicker::new(vec![
            link("docs", "https://docs.example/one"),
            link("issue tracker", "https://bugs.example/two"),
            link("https://plain.example/three", "https://plain.example/three"),
        ]);

        // Enter opens the selected link.
        assert_eq!(
            picker.handle_key(KeyCode::Enter, false),
            PickerAction::Choose("https://docs.example/one".into())
        );
        picker.handle_key(KeyCode::Down, false);
        assert_eq!(
            picker.handle_key(KeyCode::Enter, false),
            PickerAction::Choose("https://bugs.example/two".into())
        );
        // Up from the second lands on the first; Up again wraps to the last.
        picker.handle_key(KeyCode::Up, false);
        picker.handle_key(KeyCode::Up, false);
        assert_eq!(
            picker.handle_key(KeyCode::Enter, false),
            PickerAction::Choose("https://plain.example/three".into())
        );

        // Typing filters (label and url both match) and resets the selection.
        for character in "bugs".chars() {
            picker.handle_key(KeyCode::Char(character), false);
        }
        assert_eq!(
            picker.handle_key(KeyCode::Enter, false),
            PickerAction::Choose("https://bugs.example/two".into())
        );
        // No match: Enter dismisses instead of choosing.
        for character in "zzz".chars() {
            picker.handle_key(KeyCode::Char(character), false);
        }
        assert!(picker.filtered().is_empty());
        assert_eq!(
            picker.handle_key(KeyCode::Enter, false),
            PickerAction::Dismiss
        );
        // Backspace edits the query back to a match.
        for _ in 0..7 {
            picker.handle_key(KeyCode::Backspace, false);
        }
        assert_eq!(picker.filtered().len(), 3);
        assert_eq!(
            picker.handle_key(KeyCode::Esc, false),
            PickerAction::Dismiss
        );

        let mut empty = LinkPicker::new(Vec::new());
        assert_eq!(
            empty.handle_key(KeyCode::Enter, false),
            PickerAction::Dismiss
        );
    }

    #[test]
    fn link_picker_renders_filter_rows_and_click_map() {
        let picker = LinkPicker::new(vec![
            link("docs", "https://docs.example/one"),
            link("https://plain.example/two", "https://plain.example/two"),
        ]);
        let (screen, hit) = draw_modal(80, 20, |frame| render_link_picker(frame, &picker));

        assert!(screen.contains("links"), "{screen}");
        assert!(screen.contains("filter"), "{screen}");
        assert!(screen.contains("↵ open in browser"), "{screen}");
        // A labelled link shows label then url; a bare one just the url.
        assert!(
            screen.contains("> docs https://docs.example/one"),
            "{screen}"
        );
        assert!(screen.contains("  https://plain.example/two"), "{screen}");

        // The filter header is unclickable; the rows map to link indices.
        assert_eq!(hit.rows, vec![None, Some(0), Some(1)]);
        assert_eq!(hit.item_at(hit.area.x, hit.area.y), None);
        assert_eq!(hit.item_at(hit.area.x, hit.area.y + 1), Some(0));
        assert_eq!(hit.item_at(hit.area.x, hit.area.y + 2), Some(1));

        // The no-match state renders but maps no rows.
        let mut picker = picker;
        picker.handle_key(KeyCode::Char('z'), false);
        picker.handle_key(KeyCode::Char('q'), false);
        let (screen, hit) = draw_modal(80, 20, |frame| render_link_picker(frame, &picker));
        assert!(screen.contains("no matches"), "{screen}");
        assert_eq!(hit, ModalHit::default());
    }

    #[test]
    fn skill_picker_navigates_wraps_and_chooses() {
        let mut picker = SkillPicker::new(vec![
            ("deploy".into(), "Ship it".into()),
            ("review".into(), "Look closely".into()),
        ]);

        picker.handle_key(KeyCode::Down, false);
        assert_eq!(
            picker.handle_key(KeyCode::Enter, false),
            PickerAction::Choose("review".into())
        );
        // Down from the last wraps to the first; Up wraps back.
        picker.handle_key(KeyCode::Down, false);
        assert_eq!(
            picker.handle_key(KeyCode::Enter, false),
            PickerAction::Choose("deploy".into())
        );
        picker.handle_key(KeyCode::Up, false);
        assert_eq!(
            picker.handle_key(KeyCode::Enter, false),
            PickerAction::Choose("review".into())
        );
        assert_eq!(
            picker.handle_key(KeyCode::Esc, false),
            PickerAction::Dismiss
        );

        let mut empty = SkillPicker::new(Vec::new());
        assert_eq!(
            empty.handle_key(KeyCode::Enter, false),
            PickerAction::Dismiss
        );
    }

    #[test]
    fn skill_picker_renders_rows_and_click_map() {
        let picker = SkillPicker::new(vec![
            ("deploy".into(), "Ship it".into()),
            ("review".into(), "Look closely".into()),
        ]);
        let (screen, hit) = draw_modal(80, 20, |frame| render_skill_picker(frame, &picker));

        assert!(screen.contains("skills"), "{screen}");
        assert!(screen.contains("Enter insert"), "{screen}");
        assert!(screen.contains("> /deploy — Ship it"), "{screen}");
        assert!(screen.contains("  /review — Look closely"), "{screen}");

        // No header line: the first drawn row is the first skill.
        assert_eq!(hit.rows, vec![Some(0), Some(1)]);
        assert_eq!(hit.item_at(hit.area.x, hit.area.y), Some(0));
        assert_eq!(hit.item_at(hit.area.x, hit.area.y + 1), Some(1));
    }

    /// A scrolled list must keep its click map aligned with the drawn
    /// window: the first drawn row is the window start, not item 0.
    #[test]
    fn a_scrolled_skill_picker_maps_clicks_to_the_visible_window() {
        let skills: Vec<(String, String)> = (0..6)
            .map(|index| (format!("skill{index}"), format!("Description {index}")))
            .collect();
        let mut picker = SkillPicker::new(skills);
        picker.select(5);
        // An 80x8 terminal caps the modal at 4 inner rows, so selecting
        // the last of six skills scrolls the window down to items 2..=5.
        let (screen, hit) = draw_modal(80, 8, |frame| render_skill_picker(frame, &picker));
        assert!(screen.contains("> /skill5"), "{screen}");
        assert!(!screen.contains("/skill1"), "{screen}");
        assert_eq!(hit.rows, vec![Some(2), Some(3), Some(4), Some(5)]);
        assert_eq!(hit.item_at(hit.area.x, hit.area.y), Some(2));
        assert_eq!(hit.item_at(hit.area.x, hit.area.y + 3), Some(5));
    }

    #[test]
    fn turn_picker_renders_markers_armed_stakes_and_click_map() {
        let events = vec![
            meta_event(),
            checkpoint("aaa"),
            user("u1", "first question"),
            user("u2", "a steer"),
        ];
        let mut picker = TurnPicker::new(turn_entries(&events));

        // Unarmed: the tree-backed turn carries the ⎇ marker and an age.
        let (screen, hit) = draw_modal(80, 20, |frame| render_turn_picker(frame, &picker));
        assert!(screen.contains("rewind to turn"), "{screen}");
        assert!(screen.contains("filter"), "{screen}");
        assert!(screen.contains("> a steer"), "{screen}");
        assert!(screen.contains("first question"), "{screen}");
        assert!(screen.contains("⎇ now"), "{screen}");
        assert_eq!(hit.rows, vec![None, Some(0), Some(1)]);
        assert_eq!(hit.item_at(hit.area.x, hit.area.y), None);
        assert_eq!(hit.item_at(hit.area.x, hit.area.y + 1), Some(0));
        assert_eq!(hit.item_at(hit.area.x, hit.area.y + 2), Some(1));

        // Armed on a treeless turn: the right column states the stakes.
        picker.handle_key(KeyCode::Enter, false);
        let (screen, _) = draw_modal(80, 20, |frame| render_turn_picker(frame, &picker));
        assert!(screen.contains("✗ a steer"), "{screen}");
        assert!(screen.contains("↵ drops 1, chat only"), "{screen}");

        // Armed on the tree-backed turn: it promises the tree restore.
        picker.handle_key(KeyCode::Down, false);
        picker.handle_key(KeyCode::Enter, false);
        let (screen, _) = draw_modal(80, 20, |frame| render_turn_picker(frame, &picker));
        assert!(screen.contains("✗ first question"), "{screen}");
        assert!(screen.contains("↵ drops 2, restores tree"), "{screen}");
    }

    #[test]
    fn session_picker_renders_armed_deletion_and_click_map() {
        let now = std::time::SystemTime::now();
        let session = |id: &str, title: &str| ilar::session::SessionSummary {
            id: id.into(),
            title: Some(title.into()),
            modified: now,
        };
        let mut picker = SessionPicker::new(vec![
            session("aaa", "fix websearch fallback"),
            session("bbb", "voxel pagoda benchmark"),
        ]);

        let (screen, hit) = draw_modal(80, 20, |frame| render_session_picker(frame, &picker));
        assert!(screen.contains("sessions"), "{screen}");
        assert!(screen.contains("filter"), "{screen}");
        assert!(screen.contains("> fix websearch fallback"), "{screen}");
        assert!(screen.contains("  voxel pagoda benchmark"), "{screen}");
        assert!(screen.contains("now"), "{screen}");

        // The filter header is unclickable; each session row maps to the
        // filtered index the click selects.
        assert_eq!(hit.rows, vec![None, Some(0), Some(1)]);
        assert_eq!(hit.item_at(hit.area.x, hit.area.y), None);
        assert_eq!(hit.item_at(hit.area.x, hit.area.y + 1), Some(0));
        assert_eq!(hit.item_at(hit.area.x, hit.area.y + 2), Some(1));

        // A first Ctrl-D arms deletion: the row shows the confirm column.
        picker.handle_key(KeyCode::Char('d'), true);
        let (screen, hit) = draw_modal(80, 20, |frame| render_session_picker(frame, &picker));
        assert!(screen.contains("✗ fix websearch fallback"), "{screen}");
        assert!(screen.contains("^D deletes"), "{screen}");
        assert_eq!(hit.rows, vec![None, Some(0), Some(1)]);
    }

    fn search_row(session: &str, title: &str, excerpt: &str, context_line: &str) -> SearchRow {
        SearchRow {
            session_id: session.into(),
            title: title.into(),
            event: 7,
            excerpt: excerpt.into(),
            age: "3d".into(),
            context: vec![
                ("user".into(), "before the hit".into(), false),
                ("assistant".into(), context_line.into(), true),
            ],
        }
    }

    #[test]
    fn typing_restarts_the_scan_and_drops_stale_rows() {
        let mut search = SessionSearch::new();

        assert_eq!(
            search.handle_key(KeyCode::Char('a'), false),
            SessionSearchAction::Rescan
        );
        let first_generation = search.generation;
        assert!(search.scanning);
        search.push_rows(
            first_generation,
            vec![search_row("s1", "one", "a match", "ctx")],
        );
        assert_eq!(search.rows.len(), 1);

        // Another keystroke: rows clear, and the old scan's late
        // arrivals are dropped instead of mixing into the new list.
        assert_eq!(
            search.handle_key(KeyCode::Char('b'), false),
            SessionSearchAction::Rescan
        );
        assert!(search.rows.is_empty());
        search.push_rows(
            first_generation,
            vec![search_row("s1", "one", "stale", "ctx")],
        );
        assert!(search.rows.is_empty(), "stale rows accepted");
        search.finish_scan(first_generation);
        assert!(search.scanning, "a stale scan finishing ended the new one");

        // Erasing back to an empty query rescans too: empty means the
        // recent-sessions listing, not a blank pane.
        search.handle_key(KeyCode::Backspace, false);
        assert_eq!(
            search.handle_key(KeyCode::Backspace, false),
            SessionSearchAction::Rescan
        );
        assert!(search.scanning);
    }

    #[test]
    fn enter_resumes_esc_dismisses_and_ctrl_g_lists() {
        let mut search = SessionSearch::new();
        assert_eq!(
            search.handle_key(KeyCode::Enter, false),
            SessionSearchAction::Stay,
            "nothing selected, nothing resumed"
        );
        search.query = "match".into();
        search.push_rows(
            0,
            vec![
                search_row("s1", "one", "first match", "ctx"),
                search_row("s2", "two", "second match", "ctx"),
            ],
        );
        search.move_selection(1);
        assert_eq!(
            search.handle_key(KeyCode::Enter, false),
            SessionSearchAction::Resume("s2".into())
        );
        assert_eq!(
            search.handle_key(KeyCode::Esc, false),
            SessionSearchAction::Dismiss
        );
        // ^G reaches the classic list picker, where delete and fork live.
        assert_eq!(
            search.handle_key(KeyCode::Char('g'), true),
            SessionSearchAction::ListMode
        );
    }

    #[test]
    fn the_row_cap_holds_whatever_the_scanner_sends() {
        let mut search = SessionSearch::new();
        let rows = (0..MAX_SEARCH_ROWS + 50)
            .map(|index| search_row("s", "t", &format!("hit {index}"), "ctx"))
            .collect();
        search.push_rows(0, rows);
        assert_eq!(search.rows.len(), MAX_SEARCH_ROWS);
    }

    #[test]
    fn the_preview_follows_the_selection() {
        let mut search = SessionSearch::new();
        search.query = "needle".into();
        search.push_rows(
            0,
            vec![
                search_row("s1", "auth session", "needle in auth", "the auth context"),
                search_row(
                    "s2",
                    "parser session",
                    "needle in parser",
                    "the parser context",
                ),
            ],
        );

        let (screen, hit) = draw_modal(120, 24, |frame| render_session_search(frame, &search));
        assert!(screen.contains("auth session"), "{screen}");
        assert!(screen.contains("the auth context"), "{screen}");
        assert!(
            !screen.contains("the parser context"),
            "preview shows the unselected row: {screen}"
        );
        // Both rows are clickable in the left pane.
        assert!(hit.rows.iter().flatten().count() >= 2, "{hit:?}");

        search.move_selection(1);
        let (screen, _) = draw_modal(120, 24, |frame| render_session_search(frame, &search));
        assert!(screen.contains("the parser context"), "{screen}");
        assert!(!screen.contains("the auth context"), "{screen}");
    }

    #[test]
    fn the_preview_frame_covers_its_half_even_with_nothing_to_show() {
        let search = SessionSearch::new();
        let (screen, _) = draw_modal(120, 24, |frame| render_session_search(frame, &search));
        // The frame is there with its title even before any rows —
        // otherwise the transcript underneath bleeds through.
        assert!(screen.contains(" preview "), "{screen}");
    }

    #[test]
    fn search_rows_carry_their_session_age() {
        let mut search = SessionSearch::new();
        search.query = "needle".into();
        search.push_rows(0, vec![search_row("s1", "auth work", "needle here", "ctx")]);

        let (screen, _) = draw_modal(120, 24, |frame| render_session_search(frame, &search));
        assert!(screen.contains("3d"), "{screen}");
    }

    #[test]
    fn a_narrow_terminal_gets_the_list_alone() {
        let mut search = SessionSearch::new();
        search.query = "needle".into();
        search.push_rows(
            0,
            vec![search_row("s1", "one", "needle here", "context text")],
        );

        let (screen, _) = draw_modal(60, 20, |frame| render_session_search(frame, &search));
        assert!(screen.contains("needle here"), "{screen}");
        assert!(
            !screen.contains("context text"),
            "two unusable panes on a narrow terminal: {screen}"
        );
    }

    #[test]
    fn the_aside_modal_shows_question_then_answer_and_scrolls() {
        let answer = (0..40)
            .map(|index| format!("answer line {index}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let mut aside = AsideModal {
            question: "which port was it?".into(),
            answer,
            scroll: 0,
        };

        let (screen, _) = draw_modal(100, 30, |frame| {
            render_aside(frame, &aside);
            ModalHit::default()
        });
        assert!(screen.contains(" btw "), "{screen}");
        assert!(screen.contains("> which port was it?"), "{screen}");
        assert!(screen.contains("answer line 0"), "{screen}");
        assert!(!screen.contains("answer line 39"), "{screen}");

        aside.scroll = 1_000; // clamped to the tail, not past it
        let (screen, _) = draw_modal(100, 30, |frame| {
            render_aside(frame, &aside);
            ModalHit::default()
        });
        assert!(screen.contains("answer line 39"), "{screen}");
    }

    #[test]
    fn highlighted_spans_split_on_every_occurrence() {
        let spans = highlighted_spans(
            "AES table and aes TABLE",
            "aes table",
            Style::default(),
            theme::selected(),
        );
        let highlighted: Vec<&str> = spans
            .iter()
            .filter(|span| span.style == theme::selected())
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(highlighted, vec!["AES table", "aes TABLE"]);
        // No needle, one plain span.
        assert_eq!(
            highlighted_spans("text", "", Style::default(), theme::selected()).len(),
            1
        );
    }
}
