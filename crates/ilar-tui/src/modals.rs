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
    Truncation, abbreviated_path, format_tokens_compact, fuzzy_score, text_field_view,
    truncate_display,
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

/// The hover affordance modal rows owe, exactly like the transcript's:
/// when the pointer rests on a row the hit map would give a click, its
/// content gets an underline. Applied to the drawn buffer after the
/// render pass — the hit map already knows which screen rows are
/// clickable, so the individual renderers stay hover-agnostic. Leading
/// and trailing blank cells stay bare, mirroring
/// [`crate::transcript::underline_content_spans`]. Returns whether it
/// underlined, for tests.
pub(crate) fn underline_hovered_item(
    hit: &ModalHit,
    buffer: &mut ratatui::buffer::Buffer,
    column: u16,
    row: u16,
) -> bool {
    if hit.item_at(column, row).is_none() {
        return false;
    }
    let mut content =
        (hit.area.x..hit.area.right()).filter(|&x| !buffer[(x, row)].symbol().trim().is_empty());
    let Some(first) = content.next() else {
        return false;
    };
    let last = content.last().unwrap_or(first);
    for x in first..=last {
        buffer[(x, row)]
            .modifier
            .insert(ratatui::style::Modifier::UNDERLINED);
    }
    true
}

/// The chrome every picker modal shares: clear the backdrop, draw the
/// double border in the focus style with the title and the
/// right-aligned muted footer, and hand back the inner area — or
/// `None` when the terminal left no room, which is every caller's cue
/// to bail with an empty hit map. Sizes, titles, colours and footer
/// breakpoints stay with each picker; only the scaffold lives here.
pub(crate) fn modal_frame(
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
/// pickers, as `Picker::on_move` and `Picker::on_edit`.
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

/// The indices a scrolled list actually draws.
fn visible_rows(selected: usize, len: usize, row_count: usize) -> std::ops::Range<usize> {
    let start = list_window(selected, len, row_count);
    start..len.min(start.saturating_add(row_count))
}

/// The one row loop. Every modal list draws its window the same way:
/// the caller's text for the row, truncated to the width, padded back
/// out so a selected row's bar spans the modal, and pushed together
/// with the index it shows so a click lands on what it looks like.
fn push_row_window(
    body: &mut ModalRows,
    width: usize,
    rows: std::ops::Range<usize>,
    selected: usize,
    mut row: impl FnMut(usize, bool) -> (String, Style),
) {
    for index in rows {
        let (text, style) = row(index, index == selected);
        let text = truncate_display(&text, width, Truncation::Right);
        body.push(Line::styled(format!("{text:<width$}"), style), Some(index));
    }
}

/// The line the filtering pickers open with, above their rows.
fn filter_header(query: &str, width: usize) -> Line<'static> {
    Line::from(vec![
        Span::styled("filter ", Style::default().fg(MUTED)),
        Span::raw(truncate_display(
            query,
            width.saturating_sub(8),
            Truncation::Middle,
        )),
    ])
}

/// A muted note where rows would be: nothing to list, or nothing matched.
fn muted_line(text: &str) -> Line<'static> {
    Line::styled(text.to_string(), Style::default().fg(MUTED))
}

/// The line a failed switch leaves behind, in the row the picker's
/// subtitle would have used.
fn error_line(error: &str, width: usize) -> Line<'static> {
    Line::styled(
        truncate_display(error, width, Truncation::Right),
        Style::default().fg(ERROR),
    )
}

/// The marker column of the "which one is running" lists carries two
/// facts at once: where the cursor is, and which entry is in force.
fn choice_marker(selected: bool, active: bool) -> &'static str {
    match (selected, active) {
        (true, true) => ">●",
        (true, false) => "> ",
        (false, true) => " ●",
        (false, false) => "  ",
    }
}

/// One such row: the id suffix keeps the right edge and the name takes
/// whatever the marker and the suffix left.
fn marked_row(width: usize, selected: bool, active: bool, name: &str, suffix: &str) -> String {
    let marker = choice_marker(selected, active);
    let name_width = width
        .saturating_sub(UnicodeWidthStr::width(marker))
        .saturating_sub(UnicodeWidthStr::width(suffix))
        .saturating_sub(1);
    format!(
        "{marker} {}{suffix}",
        truncate_display(name, name_width, Truncation::Right)
    )
}

/// Cursor first, then the running entry's green, then plain.
fn choice_style(selected: bool, active: bool) -> Style {
    if selected {
        theme::selected()
    } else if active {
        Style::default().fg(theme::SUCCESS)
    } else {
        Style::default()
    }
}

/// The body every filtering picker draws: the "filter <query>" line,
/// and under it either the visible window of rows or the note that
/// stands in for them. The note maps no rows, so a click on "no
/// matches" selects nothing.
fn render_filtered_list(
    frame: &mut Frame,
    inner: Rect,
    query: &str,
    empty_note: &str,
    len: usize,
    selected: usize,
    row: impl FnMut(usize, bool) -> (String, Style),
) -> ModalHit {
    let width = inner.width as usize;
    let mut body = ModalRows::default();
    body.push(filter_header(query, width), None);
    if len == 0 {
        body.push(muted_line(empty_note), None);
        return body.finish_unmapped(frame, inner);
    }
    // At least one row, even on a terminal with no room for it: a
    // zero-tall window would scroll the selection off the map.
    let row_count = (inner.height as usize)
        .saturating_sub(body.line_count())
        .max(1);
    let selected = selected.min(len - 1);
    push_row_window(
        &mut body,
        width,
        visible_rows(selected, len, row_count),
        selected,
        row,
    );
    body.finish(frame, inner)
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

/// The paste half of `edit_query`: clipboard text joins a query the
/// way typed characters do. A filter is one line, so control
/// characters — a pasted newline above all — are dropped rather than
/// pushed, and tabs with them. Returns true when the query actually
/// grew, the same signal `edit_query` gives, so every caller's reset
/// hooks stay in one shape: a paste of nothing but control characters
/// must not move a selection or disarm anything.
pub(crate) fn insert_query(query: &mut String, text: &str) -> bool {
    let before = query.len();
    let room = QUERY_CAP.saturating_sub(query.chars().count());
    query.extend(
        text.chars()
            .filter(|character| !character.is_control())
            .take(room),
    );
    query.len() != before
}

/// Longest query a paste may grow: filters and search strings are a
/// line at most, and the fuzzy scorer re-runs over every candidate per
/// render — an unbounded clipboard would stall the draw loop.
const QUERY_CAP: usize = 256;

/// The fuzzy pipeline the ranking pickers share: keep what the query
/// matches, best score first. `sort_by_key` is stable, so equal scores
/// keep the input order — recency for sessions, the author's order for
/// everything else. An empty query scores everything, which is how a
/// fresh picker lists all of it.
fn fuzzy_filter<T>(
    query: &str,
    items: impl IntoIterator<Item = T>,
    haystack: impl Fn(&T) -> String,
) -> Vec<T> {
    let mut scored: Vec<(i64, T)> = items
        .into_iter()
        .filter_map(|item| fuzzy_score(query, &haystack(&item)).map(|score| (score, item)))
        .collect();
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    scored.into_iter().map(|(_, item)| item).collect()
}

/// The other filter: every whitespace-separated term must appear
/// somewhere in the item, and the list keeps its own order. The palette
/// and the model picker are curated lists where that order says more
/// than a score would.
fn term_filter<T>(
    query: &str,
    items: impl IntoIterator<Item = T>,
    haystack: impl Fn(&T) -> String,
) -> Vec<T> {
    let query = query.to_lowercase();
    let terms: Vec<&str> = query.split_whitespace().collect();
    items
        .into_iter()
        .filter(|item| {
            let haystack = haystack(item).to_lowercase();
            terms.iter().all(|term| haystack.contains(term))
        })
        .collect()
}

/// The picker skeleton, written once. Nine modals used to carry their
/// own copy of it: the same cursor wrappers, the same key order
/// (navigate → dismiss → choose → edit the filter), the same
/// reset-on-edit. What genuinely differs stays in the picker — what it
/// holds (`row_count`), what its action enum is called (`stay`,
/// `dismiss`, `choose`), what a moved cursor or a changed query must
/// clear (`on_move`, `on_edit`), and which extra chords it answers
/// (matched in its own `handle_key`, ahead of `skeleton_key`).
///
/// The wrappers `App` calls — `select`, `move_selection`,
/// `insert_query`, `handle_key` — stay inherent methods on each picker,
/// because `app.rs` never imports this trait. They are one-liners over
/// `nav_to`, `nav_by`, `paste_query` and `skeleton_key`.
trait Picker {
    /// The picker's own action enum.
    type Action;

    fn nav(&mut self) -> &mut ListNav;

    /// Rows the cursor moves within: the *filtered* count, which
    /// changes under the cursor as the query is typed.
    fn row_count(&self) -> usize;

    /// A key that changed nothing the caller has to act on.
    fn stay(&self) -> Self::Action;

    fn dismiss(&self) -> Self::Action;

    /// Enter on the current selection. With nothing selected every
    /// picker stays: a query matching nothing is a typo, and throwing
    /// the modal away over one is Esc's job, not Enter's.
    fn choose(&mut self) -> Self::Action;

    /// The filter, for the pickers that have one. `None` means keys
    /// that would type into it are ignored — the skill picker lists the
    /// whole inventory and the reasoning picker a handful of levels, so
    /// neither has anything to narrow.
    fn query(&mut self) -> Option<&mut String> {
        None
    }

    /// The cursor moved: disarm whatever was armed for the old row.
    fn on_move(&mut self) {}

    /// The query grew or shrank. The default jumps back to the best
    /// match and disarms with it; pickers that rescan or re-anchor
    /// instead override this.
    fn on_edit(&mut self) -> Self::Action {
        self.nav().reset();
        self.on_move();
        self.stay()
    }

    /// Click-to-select: the index comes from the frame's hit map, which
    /// is a frame out of date by the time a click arrives, so
    /// `ListNav::select` clamps it.
    /// The count is read before the hook runs, so no `on_move` may
    /// change the number of rows.
    fn nav_to(&mut self, index: usize) {
        let len = self.row_count();
        self.on_move();
        self.nav().select(index, len);
    }

    /// Arrow keys and the wheel, wrapping around the list. Reads the
    /// count before the hook, like `nav_to`.
    fn nav_by(&mut self, delta: isize) {
        let len = self.row_count();
        self.on_move();
        self.nav().move_by(delta, len);
    }

    /// Clipboard text joins the filter the way typed characters do. A
    /// paste that adds nothing — all control characters, or a query
    /// already at the cap — must not move a selection or disarm
    /// anything, so it reports `stay`.
    fn paste_query(&mut self, text: &str) -> Self::Action {
        let grew = self.query().is_some_and(|query| insert_query(query, text));
        if grew { self.on_edit() } else { self.stay() }
    }

    /// Navigate, dismiss, choose, or edit the filter — in that order.
    /// Chords a picker claims for itself are matched before it delegates
    /// here, so they never reach the query editor.
    fn skeleton_key(&mut self, code: KeyCode, control: bool) -> Self::Action {
        if let Some(delta) = nav_delta(code, control) {
            self.nav_by(delta);
            return self.stay();
        }
        match (code, control) {
            (KeyCode::Esc, _) => self.dismiss(),
            (KeyCode::Enter, _) => self.choose(),
            _ => {
                let edited = self
                    .query()
                    .is_some_and(|query| edit_query(query, code, control));
                if edited { self.on_edit() } else { self.stay() }
            }
        }
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

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PaletteItem {
    command: PaletteCommand,
    label: String,
    shortcut: String,
    search_terms: String,
}

/// Palette entries, one per built-in command.
pub(crate) fn palette_items() -> Vec<PaletteItem> {
    PALETTE_COMMANDS
        .iter()
        .map(|command| PaletteItem {
            command: command.id,
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
    Choose(PaletteCommand),
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
        term_filter(&self.query, self.items.iter(), |item| {
            format!("{} {} {}", item.label, item.shortcut, item.search_terms)
        })
    }

    /// Click-to-select: the index is into the filtered list.
    pub(crate) fn select(&mut self, index: usize) {
        self.nav_to(index);
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        self.nav_by(delta);
    }

    pub(crate) fn insert_query(&mut self, text: &str) {
        self.paste_query(text);
    }

    pub(crate) fn handle_key(&mut self, code: KeyCode, control: bool) -> CommandPaletteAction {
        match (code, control) {
            (KeyCode::Home, _) => {
                self.nav.reset();
                CommandPaletteAction::Stay
            }
            (KeyCode::End, _) => {
                self.nav.selected = self.row_count().saturating_sub(1);
                CommandPaletteAction::Stay
            }
            _ => self.skeleton_key(code, control),
        }
    }
}

impl Picker for CommandPalette {
    type Action = CommandPaletteAction;

    fn nav(&mut self) -> &mut ListNav {
        &mut self.nav
    }

    fn row_count(&self) -> usize {
        self.filtered_commands().len()
    }

    fn stay(&self) -> Self::Action {
        CommandPaletteAction::Stay
    }

    fn dismiss(&self) -> Self::Action {
        CommandPaletteAction::Dismiss
    }

    fn choose(&mut self) -> Self::Action {
        self.filtered_commands()
            .get(self.nav.selected)
            .map(|item| CommandPaletteAction::Choose(item.command))
            .unwrap_or(CommandPaletteAction::Stay)
    }

    fn query(&mut self) -> Option<&mut String> {
        Some(&mut self.query)
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
            binding!("Ctrl-S", "stash the draft · pops it back when blank"),
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
            binding!("Ctrl-L", "repaint the screen (works with an overlay up)"),
            binding!("PgUp / PgDn", "scroll page"),
            binding!("Alt-U / Alt-D", "scroll half page"),
            binding!("Ctrl-Home / Ctrl-End", "jump to top / tail"),
            binding!("Up / Down", "scroll line (at the edges of the draft)"),
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
            Paragraph::new(muted_line(
                "nothing pending — queued messages, the goal, background jobs, and retry offers appear here",
            )),
            inner,
        );
        return ModalHit::default();
    }
    let width = inner.width as usize;
    let selected = snapshot.selected.min(snapshot.rows.len().saturating_sub(1));
    let mut body = ModalRows::default();
    push_row_window(
        &mut body,
        width,
        visible_rows(selected, snapshot.rows.len(), inner.height as usize),
        selected,
        |index, is_selected| {
            let armed = snapshot.armed && is_selected;
            let marker = if is_selected {
                if armed { "✗ " } else { "> " }
            } else {
                "  "
            };
            let style = if armed {
                // Armed deletion is the one place a full bar is the
                // point; it is still a colour, not inverted video.
                Style::default().fg(theme::SELECTED_FG).bg(ERROR)
            } else if is_selected {
                theme::selected()
            } else {
                Style::default().fg(theme::PRIMARY)
            };
            (format!("{marker}{}", snapshot.rows[index]), style)
        },
    );
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
    render_scrolled(
        frame,
        inner,
        help_lines(inner.width as usize, keyboard_enhanced),
        scroll,
    );
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
    render_scrolled(
        frame,
        inner,
        todo_overlay_lines(list, inner.width as usize),
        scroll,
    );
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
    render_scrolled(frame, inner, lines, aside.scroll);
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
    nav: ListNav,
    pub(crate) armed: Option<PendingItem>,
}

impl PendingManager {
    pub(crate) fn selected(&self) -> usize {
        self.nav.selected
    }

    /// Re-clamp: items come and go under the cursor while the modal is
    /// open, so the list can shrink past it between keys.
    pub(crate) fn clamp(&mut self, len: usize) {
        self.nav.select(self.nav.selected, len);
    }

    /// Click-to-select: the index comes from the frame's hit map.
    /// Disarms, like any other selection change.
    pub(crate) fn select(&mut self, index: usize, len: usize) {
        self.nav.select(index, len);
        self.armed = None;
    }

    /// Arrow keys wrap around the list, and moving disarms.
    pub(crate) fn move_selection(&mut self, delta: isize, len: usize) {
        self.nav.move_by(delta, len);
        self.armed = None;
    }
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
        self.nav_to(index);
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        self.nav_by(delta);
    }

    pub(crate) fn handle_key(&mut self, code: KeyCode, control: bool) -> PickerAction {
        self.skeleton_key(code, control)
    }
}

/// The whole inventory is always listed, so there is no query: typed
/// characters fall through to `stay`.
impl Picker for SkillPicker {
    type Action = PickerAction;

    fn nav(&mut self) -> &mut ListNav {
        &mut self.nav
    }

    fn row_count(&self) -> usize {
        self.skills.len()
    }

    fn stay(&self) -> Self::Action {
        PickerAction::Stay
    }

    fn dismiss(&self) -> Self::Action {
        PickerAction::Dismiss
    }

    fn choose(&mut self) -> Self::Action {
        self.skills
            .get(self.nav.selected)
            .map(|(name, _)| PickerAction::Choose(name.clone()))
            .unwrap_or(PickerAction::Stay)
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
    let mut body = ModalRows::default();
    push_row_window(
        &mut body,
        inner.width as usize,
        visible_rows(selected, picker.skills.len(), inner.height as usize),
        selected,
        |index, is_selected| {
            let (name, description) = &picker.skills[index];
            let marker = if is_selected { "> " } else { "  " };
            let style = if is_selected {
                theme::selected()
            } else {
                Style::default().fg(theme::PRIMARY)
            };
            (format!("{marker}/{name} — {description}"), style)
        },
    );
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
    /// The directory ilar is running in, canonicalized by the caller.
    /// Sessions started here lead the unfiltered list, so the row the
    /// picker opens on is where the user left off *here*.
    cwd: Option<std::path::PathBuf>,
    home: Option<std::path::PathBuf>,
    query: String,
    pub(crate) nav: ListNav,
    /// Session id armed for deletion; the next Ctrl-D confirms.
    pending_delete: Option<String>,
}

impl SessionPicker {
    pub(crate) fn new(
        sessions: Vec<ilar::session::SessionSummary>,
        cwd: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            sessions,
            cwd,
            home: std::env::var_os("HOME").map(std::path::PathBuf::from),
            query: String::new(),
            nav: ListNav::default(),
            pending_delete: None,
        }
    }

    fn origin(&self, session: &ilar::session::SessionSummary) -> RowOrigin {
        row_origin(
            session.cwd.as_deref(),
            self.cwd.as_deref(),
            self.home.as_deref(),
        )
    }

    /// Sessions matching the query, best fuzzy score first (stable, so
    /// equal scores keep recency order). With no query the listing is a
    /// resume list rather than a search result, so this directory's
    /// sessions lead it — recency within each group.
    fn filtered(&self) -> Vec<&ilar::session::SessionSummary> {
        let matched = fuzzy_filter(&self.query, self.sessions.iter(), |session| {
            format!("{} {}", session.title.as_deref().unwrap_or(""), session.id)
        });
        if self.query.trim().is_empty() {
            here_first(matched, |session| {
                launched_here(session.cwd.as_deref(), self.cwd.as_deref())
            })
        } else {
            matched
        }
    }

    fn selected_id(&self) -> Option<String> {
        self.filtered()
            .get(self.nav.selected)
            .map(|session| session.id.clone())
    }

    /// Click-to-select. Disarms a pending delete, like any other
    /// selection move.
    pub(crate) fn select(&mut self, index: usize) {
        self.nav_to(index);
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        self.nav_by(delta);
    }

    pub(crate) fn insert_query(&mut self, text: &str) {
        self.paste_query(text);
    }

    pub(crate) fn handle_key(&mut self, code: KeyCode, control: bool) -> SessionPickerAction {
        match (code, control) {
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
            _ => self.skeleton_key(code, control),
        }
    }
}

impl Picker for SessionPicker {
    type Action = SessionPickerAction;

    fn nav(&mut self) -> &mut ListNav {
        &mut self.nav
    }

    fn row_count(&self) -> usize {
        self.filtered().len()
    }

    fn stay(&self) -> Self::Action {
        SessionPickerAction::Stay
    }

    fn dismiss(&self) -> Self::Action {
        SessionPickerAction::Dismiss
    }

    fn choose(&mut self) -> Self::Action {
        self.selected_id()
            .map(SessionPickerAction::Resume)
            .unwrap_or(SessionPickerAction::Stay)
    }

    fn query(&mut self) -> Option<&mut String> {
        Some(&mut self.query)
    }

    fn on_move(&mut self) {
        self.pending_delete = None;
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
    /// Event index of the best hit inside its session, shown as an
    /// anchor in query mode.
    pub(crate) event: usize,
    /// When the session was last used, stamped by [`last_used`].
    pub(crate) age: String,
    /// Whether the session belongs to the directory ilar runs in, and
    /// which directory it belongs to otherwise.
    pub(crate) origin: RowOrigin,
    /// Query mode: how many places matched in this session, bounded by
    /// the scan's per-session cap. Zero on the plain listing.
    pub(crate) match_count: usize,
    /// The query matched the session's title; ranked ahead of
    /// content-only matches.
    pub(crate) title_match: bool,
    /// (speaker label, text, is-the-hit) around the match, in order.
    /// Empty on a listing row until the lazy preview loads it.
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
    /// Whether the user has taken the cursor somewhere of their own.
    /// Until they do, the still-streaming listing may re-order under it
    /// — the top row is the answer, whichever session that turns out to
    /// be. Afterwards the cursor follows the row it is on.
    steered: bool,
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
            steered: false,
        }
    }

    pub(crate) fn selected(&self) -> Option<&SearchRow> {
        self.rows.get(self.nav.selected)
    }

    pub(crate) fn select(&mut self, index: usize) {
        self.nav_to(index);
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        self.nav_by(delta);
    }

    /// Accept a batch from the scanner, unless it answers a query the
    /// user has already typed past.
    ///
    /// With no query the rows are a resume listing, not search results,
    /// so this directory's sessions are kept at the top as they arrive
    /// — the scan streams in recency order, and the partition is stable,
    /// so each group stays in it. A typed query is ordered by the scan's
    /// match quality and left exactly as delivered.
    ///
    /// Hoisting a late arrival past a cursor the user placed would leave
    /// the highlight on a different session than the one it was on, so
    /// once they have steered, the selection follows its row.
    pub(crate) fn push_rows(&mut self, generation: u64, rows: Vec<SearchRow>) {
        if generation != self.generation {
            return;
        }
        let anchor = self
            .steered
            .then(|| {
                self.selected()
                    .map(|row| (row.session_id.clone(), row.event))
            })
            .flatten();
        let room = MAX_SEARCH_ROWS.saturating_sub(self.rows.len());
        self.rows.extend(rows.into_iter().take(room));
        if self.query.trim().is_empty() {
            self.rows = here_first(std::mem::take(&mut self.rows), |row| row.origin.here());
        } else {
            // Search order: a session named for the query beats one
            // that merely mentions it. Stable, so each group keeps the
            // scan's recency order.
            self.rows = here_first(std::mem::take(&mut self.rows), |row| row.title_match);
        }
        if let Some((session_id, event)) = anchor
            && let Some(index) = self
                .rows
                .iter()
                .position(|row| row.session_id == session_id && row.event == event)
        {
            self.nav.selected = index;
        }
    }

    /// The scan for `generation` has no more rows to deliver.
    pub(crate) fn finish_scan(&mut self, generation: u64) {
        if generation == self.generation {
            self.scanning = false;
        }
    }

    /// Pasted text extends the grep query, which invalidates whatever
    /// the running scan is about to deliver.
    pub(crate) fn insert_query(&mut self, text: &str) -> SessionSearchAction {
        self.paste_query(text)
    }

    pub(crate) fn handle_key(&mut self, code: KeyCode, control: bool) -> SessionSearchAction {
        match (code, control) {
            (KeyCode::Char('g'), true) => SessionSearchAction::ListMode,
            _ => self.skeleton_key(code, control),
        }
    }
}

impl Picker for SessionSearch {
    type Action = SessionSearchAction;

    fn nav(&mut self) -> &mut ListNav {
        &mut self.nav
    }

    /// The rows are whatever the scan delivered; there is no local
    /// filter to apply, because the query is the scan.
    fn row_count(&self) -> usize {
        self.rows.len()
    }

    fn stay(&self) -> Self::Action {
        SessionSearchAction::Stay
    }

    fn dismiss(&self) -> Self::Action {
        SessionSearchAction::Dismiss
    }

    fn choose(&mut self) -> Self::Action {
        self.selected()
            .map(|row| SessionSearchAction::Resume(row.session_id.clone()))
            .unwrap_or(SessionSearchAction::Stay)
    }

    fn query(&mut self) -> Option<&mut String> {
        Some(&mut self.query)
    }

    /// A cursor the user placed is theirs to keep: streamed rows stop
    /// re-ordering under it.
    fn on_move(&mut self) {
        self.steered = true;
    }

    /// Editing the query does not re-filter a list here, it retires
    /// one: the rows on screen answer the old query, and the scan
    /// still delivering them is stamped stale.
    fn on_edit(&mut self) -> Self::Action {
        self.nav.reset();
        self.rows.clear();
        self.steered = false;
        self.generation += 1;
        // Empty query included: that rescans as the recent-sessions
        // listing.
        self.scanning = true;
        SessionSearchAction::Rescan
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
        fuzzy_filter(&self.query, self.links.iter(), |link| {
            format!("{} {}", link.label, link.url)
        })
    }

    pub(crate) fn select(&mut self, index: usize) {
        self.nav_to(index);
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        self.nav_by(delta);
    }

    pub(crate) fn insert_query(&mut self, text: &str) {
        self.paste_query(text);
    }

    pub(crate) fn handle_key(&mut self, code: KeyCode, control: bool) -> PickerAction {
        self.skeleton_key(code, control)
    }
}

impl Picker for LinkPicker {
    type Action = PickerAction;

    fn nav(&mut self) -> &mut ListNav {
        &mut self.nav
    }

    fn row_count(&self) -> usize {
        self.filtered().len()
    }

    fn stay(&self) -> Self::Action {
        PickerAction::Stay
    }

    fn dismiss(&self) -> Self::Action {
        PickerAction::Dismiss
    }

    fn choose(&mut self) -> Self::Action {
        self.filtered()
            .get(self.nav.selected)
            .map(|link| PickerAction::Choose(link.url.clone()))
            .unwrap_or(PickerAction::Stay)
    }

    fn query(&mut self) -> Option<&mut String> {
        Some(&mut self.query)
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
    let width = inner.width as usize;
    let links = picker.filtered();
    render_filtered_list(
        frame,
        inner,
        &picker.query,
        if picker.links.is_empty() {
            "no links in this transcript"
        } else {
            "no matches"
        },
        links.len(),
        picker.nav.selected,
        |index, is_selected| {
            let link = links[index];
            let marker = if is_selected { "> " } else { "  " };
            // A bare url is its own label; middle truncation keeps the
            // host and the tail, which is what identifies a link.
            let text = if link.label == link.url {
                truncate_display(&format!("{marker}{}", link.url), width, Truncation::Middle)
            } else {
                let url_budget = width.saturating_sub(
                    UnicodeWidthStr::width(marker)
                        + UnicodeWidthStr::width(link.label.as_str())
                        + 1,
                );
                let url = truncate_display(&link.url, url_budget, Truncation::Middle);
                truncate_display(
                    &format!("{marker}{} {url}", link.label),
                    width,
                    Truncation::Middle,
                )
            };
            let style = if is_selected {
                theme::selected()
            } else {
                Style::default().fg(theme::PRIMARY)
            };
            (text, style)
        },
    )
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
        fuzzy_filter(&self.query, self.turns.iter(), |turn| turn.excerpt.clone())
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
        self.nav_to(index);
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        self.nav_by(delta);
    }

    pub(crate) fn insert_query(&mut self, text: &str) {
        self.paste_query(text);
    }

    pub(crate) fn handle_key(&mut self, code: KeyCode, control: bool) -> TurnPickerAction {
        match (code, control) {
            (KeyCode::Char('y'), true) => self
                .selected_turn()
                .map(|turn| TurnPickerAction::Fork {
                    cut: turn.cut,
                    target: turn.user_id.clone(),
                })
                .unwrap_or(TurnPickerAction::Stay),
            _ => self.skeleton_key(code, control),
        }
    }
}

impl Picker for TurnPicker {
    type Action = TurnPickerAction;

    fn nav(&mut self) -> &mut ListNav {
        &mut self.nav
    }

    fn row_count(&self) -> usize {
        self.filtered().len()
    }

    fn stay(&self) -> Self::Action {
        TurnPickerAction::Stay
    }

    fn dismiss(&self) -> Self::Action {
        TurnPickerAction::Dismiss
    }

    /// Rewind is destructive, so Enter arms the row and the next Enter
    /// on the same row fires it.
    fn choose(&mut self) -> Self::Action {
        match self.selected_turn() {
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
            None => TurnPickerAction::Stay,
        }
    }

    fn query(&mut self) -> Option<&mut String> {
        Some(&mut self.query)
    }

    fn on_move(&mut self) {
        self.armed = None;
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
    let width = inner.width as usize;
    let now = std::time::SystemTime::now();
    let turns = picker.filtered();
    render_filtered_list(
        frame,
        inner,
        &picker.query,
        if picker.turns.is_empty() {
            "no turns to rewind to"
        } else {
            "no matches"
        },
        turns.len(),
        picker.nav.selected,
        |index, is_selected| {
            let turn = turns[index];
            let armed = is_selected && picker.armed.as_deref() == Some(turn.user_id.as_str());
            let marker = if is_selected {
                if armed { "✗ " } else { "> " }
            } else {
                "  "
            };
            // The right column is the age, until the row is armed —
            // then it states what the confirming Enter would cost.
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
            let label_width = width
                .saturating_sub(UnicodeWidthStr::width(marker))
                .saturating_sub(UnicodeWidthStr::width(right.as_str()))
                .saturating_sub(1);
            let label = truncate_display(&turn.excerpt, label_width, Truncation::Right);
            let style = if armed {
                Style::default().fg(theme::SELECTED_FG).bg(theme::ERROR)
            } else if is_selected {
                theme::selected()
            } else {
                Style::default().fg(theme::PRIMARY)
            };
            (format!("{marker}{label:<label_width$} {right}"), style)
        },
    )
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

/// When a session was last used, as the resume surfaces stamp it.
/// Inside a day the distance is what matters ("35m ago"); past that it
/// stops meaning anything and the date is what identifies the session,
/// with the year only when it is not this one. `now` is a parameter so
/// the buckets are the same wherever this is called from — and testable
/// without a clock.
pub(crate) fn last_used(modified: std::time::SystemTime, now: std::time::SystemTime) -> String {
    use chrono::Datelike;
    let seconds = now.duration_since(modified).unwrap_or_default().as_secs();
    match seconds {
        0..=59 => "just now".into(),
        60..=3_599 => format!("{}m ago", seconds / 60),
        3_600..=86_399 => format!("{}h ago", seconds / 3_600),
        _ => {
            let when = chrono::DateTime::<chrono::Local>::from(modified);
            if when.year() == chrono::DateTime::<chrono::Local>::from(now).year() {
                when.format("%b %-d").to_string().to_lowercase()
            } else {
                when.format("%Y-%m-%d").to_string()
            }
        }
    }
}

/// Where a listed session was last used, relative to where ilar is
/// running now. Only rows that are `Here` lead the unfiltered listing;
/// everything else is marked with its directory, when it recorded one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RowOrigin {
    /// The directory ilar is running in — the row needs no marker,
    /// because it is where the user already is.
    Here,
    /// Another directory, abbreviated for display; `None` for sessions
    /// written before the workspace was recorded.
    Elsewhere(Option<String>),
}

impl Default for RowOrigin {
    /// Knowing nothing is knowing it is not this directory.
    fn default() -> Self {
        Self::Elsewhere(None)
    }
}

impl RowOrigin {
    fn here(&self) -> bool {
        matches!(self, Self::Here)
    }

    fn directory(&self) -> Option<&str> {
        match self {
            Self::Here => None,
            Self::Elsewhere(directory) => directory.as_deref(),
        }
    }
}

/// Whether a session was launched from the directory ilar runs in.
/// The comparison is exact: both paths are canonical (the session
/// canonicalized its launch directory when it recorded it, and so did
/// the workspace whose cwd the caller passes), so a subdirectory of
/// this checkout is another directory, not this one. A session that
/// recorded no directory is never here — the ordering says so, without
/// a path to show for it.
pub(crate) fn launched_here(
    launched_in: Option<&std::path::Path>,
    cwd: Option<&std::path::Path>,
) -> bool {
    matches!((launched_in, cwd), (Some(launched_in), Some(cwd)) if launched_in == cwd)
}

/// Classify one row's launch directory against the directory ilar runs
/// in, abbreviating the directory it will be marked with when it is not
/// this one. Sorting asks [`launched_here`] instead: it needs the
/// verdict, not the label.
pub(crate) fn row_origin(
    launched_in: Option<&std::path::Path>,
    cwd: Option<&std::path::Path>,
    home: Option<&std::path::Path>,
) -> RowOrigin {
    if launched_here(launched_in, cwd) {
        RowOrigin::Here
    } else {
        RowOrigin::Elsewhere(launched_in.map(|launched_in| abbreviated_path(launched_in, home)))
    }
}

/// "This directory first", stably: the rows from here keep their
/// order — recency, as the listing delivered it — and the rest follow
/// in theirs.
fn here_first<T>(rows: Vec<T>, here: impl Fn(&T) -> bool) -> Vec<T> {
    let (mine, others): (Vec<T>, Vec<T>) = rows.into_iter().partition(here);
    mine.into_iter().chain(others).collect()
}

/// The right-hand column of a resume row: when the session was last
/// used, behind its directory when that is not the one ilar runs in.
/// The directory gets at most a third of the row and is truncated in
/// the middle, which keeps the leaf — the part that names the project.
/// Under that it is dropped rather than shrunk further: the column
/// dates the row first and places it second, and a row too narrow for
/// both keeps the date.
fn resume_column(origin: &RowOrigin, stamp: &str, width: usize) -> String {
    let budget = (width / 3).min(28);
    match origin.directory().filter(|_| budget >= 8) {
        Some(directory) => format!(
            "· {} {stamp}",
            truncate_display(directory, budget, Truncation::Middle)
        ),
        None => stamp.to_string(),
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
    let width = inner.width as usize;
    let now = std::time::SystemTime::now();
    let sessions = picker.filtered();
    render_filtered_list(
        frame,
        inner,
        &picker.query,
        if picker.sessions.is_empty() {
            "no other sessions"
        } else {
            "no matches"
        },
        sessions.len(),
        picker.nav.selected,
        |index, is_selected| {
            let session = sessions[index];
            let armed =
                is_selected && picker.pending_delete.as_deref() == Some(session.id.as_str());
            let marker = if !is_selected {
                "  "
            } else if armed {
                "✗ "
            } else {
                "> "
            };
            let age = if armed {
                "^D deletes".to_string()
            } else {
                resume_column(
                    &picker.origin(session),
                    &last_used(session.modified, now),
                    width,
                )
            };
            let title = session.title.as_deref().unwrap_or("(no messages yet)");
            let label_width = width
                .saturating_sub(UnicodeWidthStr::width(marker))
                .saturating_sub(UnicodeWidthStr::width(age.as_str()))
                .saturating_sub(1);
            let label = truncate_display(title, label_width, Truncation::Right);
            let style = if is_selected {
                theme::selected()
            } else {
                Style::default().fg(theme::PRIMARY)
            };
            (format!("{marker}{label:<label_width$} {age}"), style)
        },
    )
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
            .map(|row| {
                if row.match_count > 0 {
                    format!(" event {} · {} ", row.event, row.age)
                } else {
                    format!(" {} ", row.age)
                }
            })
            .unwrap_or_default();
        if let Some(preview_inner) =
            modal_frame(frame, preview_area, &title, theme::PRIMARY, &footer)
            && let Some(row) = row
        {
            let mut lines: Vec<Line> = Vec::new();
            if row.context.is_empty() {
                lines.push(Line::styled("loading preview…", Style::default().fg(MUTED)));
            }
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
        body.push(muted_line(hint), None);
        return body.finish_unmapped(frame, inner);
    }

    // Two lines per session: the title owns the first, everything
    // else sits dim on the second. The excerpt is gone — the preview
    // pane is what an excerpt was pretending to be.
    let row_count = ((inner.height as usize)
        .saturating_sub(body.line_count())
        .max(2))
        / 2;
    let width = inner.width as usize;
    for index in visible_rows(selected, search.rows.len(), row_count) {
        let row = &search.rows[index];
        let marker = if index == selected { "> " } else { "  " };
        let title = truncate_display(&row.title, width.saturating_sub(2), Truncation::Right);
        let mut details = String::new();
        if let Some(directory) = row.origin.directory() {
            details.push_str(&truncate_display(
                directory,
                (width / 2).max(8),
                Truncation::Middle,
            ));
            details.push_str(" · ");
        }
        details.push_str(&row.age);
        if row.match_count == 1 {
            details.push_str(" · 1 match");
        } else if row.match_count > 1 {
            details.push_str(&format!(" · {} matches", row.match_count));
        }
        let details = truncate_display(&details, width.saturating_sub(4), Truncation::Right);
        if index == selected {
            // The bar owns both rows; per-span colours would fight it.
            let pad = width.saturating_sub(2 + UnicodeWidthStr::width(title.as_str()));
            body.push(
                Line::styled(
                    format!("{marker}{title}{}", " ".repeat(pad)),
                    theme::selected(),
                ),
                Some(index),
            );
            let pad = width.saturating_sub(4 + UnicodeWidthStr::width(details.as_str()));
            body.push(
                Line::styled(
                    format!("    {details}{}", " ".repeat(pad)),
                    theme::selected(),
                ),
                Some(index),
            );
        } else {
            let mut spans = vec![Span::raw(marker.to_string())];
            spans.extend(highlighted_spans(
                &title,
                &search.query,
                Style::default().fg(theme::PRIMARY),
                theme::title(theme::MARKUP),
            ));
            body.push(Line::from(spans), Some(index));
            body.push(
                Line::styled(format!("    {details}"), Style::default().fg(MUTED)),
                Some(index),
            );
        }
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
        term_filter(&self.query, self.models.iter().copied(), |model| {
            format!("{} {} {}", model.provider, model.id, model.name)
        })
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
        self.nav_to(index);
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        self.nav_by(delta);
    }

    pub(crate) fn insert_query(&mut self, text: &str) {
        self.paste_query(text);
    }

    fn select_boundary(&mut self, end: bool) {
        self.nav.selected = if end {
            self.row_count().saturating_sub(1)
        } else {
            0
        };
    }

    pub(crate) fn handle_key(&mut self, code: KeyCode, control: bool) -> PickerAction {
        match (code, control) {
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
            _ => self.skeleton_key(code, control),
        }
    }
}

impl Picker for ModelPicker {
    type Action = PickerAction;

    fn nav(&mut self) -> &mut ListNav {
        &mut self.nav
    }

    fn row_count(&self) -> usize {
        self.filtered_models().len()
    }

    fn stay(&self) -> Self::Action {
        PickerAction::Stay
    }

    fn dismiss(&self) -> Self::Action {
        PickerAction::Dismiss
    }

    /// Choosing the model already running is a no-op unless it has
    /// reasoning variants to descend into, so it just closes.
    fn choose(&mut self) -> Self::Action {
        self.filtered_models()
            .get(self.nav.selected)
            .map(|model| {
                let id = model.full_id();
                if id == self.active_model && model.variants().is_empty() {
                    PickerAction::Dismiss
                } else {
                    PickerAction::Choose(id)
                }
            })
            .unwrap_or(PickerAction::Stay)
    }

    fn query(&mut self) -> Option<&mut String> {
        Some(&mut self.query)
    }

    /// The error is about the model the last Enter tried to switch to;
    /// re-filtering retires it, but merely moving the cursor does not.
    fn on_edit(&mut self) -> Self::Action {
        self.nav.reset();
        self.error = None;
        PickerAction::Stay
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
        self.nav_to(index);
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        self.nav_by(delta);
    }

    fn selected_variant(&self) -> Option<String> {
        self.nav
            .selected
            .checked_sub(1)
            .and_then(|index| self.model.variants().get(index))
            .map(|variant| variant.id.to_string())
    }

    pub(crate) fn handle_key(&mut self, code: KeyCode, control: bool) -> VariantPickerAction {
        match (code, control) {
            (KeyCode::Home, _) => {
                self.nav.reset();
                VariantPickerAction::Stay
            }
            (KeyCode::End, _) => {
                self.nav.selected = self.model.variants().len();
                VariantPickerAction::Stay
            }
            _ => self.skeleton_key(code, control),
        }
    }
}

/// A handful of fixed levels: no query, and always a selection.
impl Picker for VariantPicker {
    type Action = VariantPickerAction;

    fn nav(&mut self) -> &mut ListNav {
        &mut self.nav
    }

    fn row_count(&self) -> usize {
        self.choice_count()
    }

    fn stay(&self) -> Self::Action {
        VariantPickerAction::Stay
    }

    fn dismiss(&self) -> Self::Action {
        VariantPickerAction::Dismiss
    }

    /// Re-choosing the running level changes nothing, so it closes.
    fn choose(&mut self) -> Self::Action {
        let selected = self.selected_variant();
        if selected == self.active_variant {
            VariantPickerAction::Dismiss
        } else {
            VariantPickerAction::Choose(selected)
        }
    }

    fn on_move(&mut self) {
        self.error = None;
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
        let ranked = fuzzy_filter(&self.query, theme::ThemeId::ALL, |candidate| {
            format!("{} {}", candidate.label(), candidate.id())
        });
        self.matches = if ranked.is_empty() {
            theme::ThemeId::ALL.to_vec()
        } else {
            ranked
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
        self.nav_to(selected);
        ThemePickerAction::Preview(self.selected_theme())
    }

    pub(crate) fn move_selection(&mut self, delta: isize) -> ThemePickerAction {
        self.nav_by(delta);
        ThemePickerAction::Preview(self.selected_theme())
    }

    /// Like the typed path, the selection is not reset: `refresh()`
    /// re-anchors it on whatever theme was highlighted.
    pub(crate) fn insert_query(&mut self, text: &str) -> ThemePickerAction {
        self.paste_query(text)
    }

    pub(crate) fn handle_key(&mut self, code: KeyCode, control: bool) -> ThemePickerAction {
        match (code, control) {
            (KeyCode::Home, _) => self.select(0),
            (KeyCode::End, _) => self.select(self.matches.len().saturating_sub(1)),
            _ => self.skeleton_key(code, control),
        }
    }
}

/// The one picker whose every key is an answer: it previews live, so
/// even a key that does nothing reports what is highlighted.
impl Picker for ThemePicker {
    type Action = ThemePickerAction;

    fn nav(&mut self) -> &mut ListNav {
        &mut self.nav
    }

    fn row_count(&self) -> usize {
        self.matches.len()
    }

    fn stay(&self) -> Self::Action {
        ThemePickerAction::Preview(self.selected_theme())
    }

    fn dismiss(&self) -> Self::Action {
        ThemePickerAction::Dismiss
    }

    fn choose(&mut self) -> Self::Action {
        ThemePickerAction::Choose(self.selected_theme())
    }

    fn query(&mut self) -> Option<&mut String> {
        Some(&mut self.query)
    }

    fn on_move(&mut self) {
        // Re-anchor the cursor first: it is only clamped on read.
        self.nav.selected = self.selected_index();
        self.error = None;
    }

    /// Unlike every other picker, a filter edit does not jump to the
    /// top: `refresh` re-ranks and then finds the highlighted theme
    /// again, so narrowing the query never previews something else.
    fn on_edit(&mut self) -> Self::Action {
        self.refresh()
    }
}

/// Draw a read-only body at a scroll offset. The offset is clamped to
/// the last full screen, so a stale scroll from a taller frame cannot
/// leave the modal blank.
fn render_scrolled(frame: &mut Frame, inner: Rect, lines: Vec<Line<'_>>, scroll: usize) {
    let start = scroll.min(lines.len().saturating_sub(inner.height as usize));
    let visible = lines
        .into_iter()
        .skip(start)
        .take(inner.height as usize)
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(visible), inner);
}

pub(crate) fn centered_rect(area: Rect, max_width: u16, max_height: u16) -> Rect {
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
            body.push(muted_line(" no matching commands"), None);
        }
    } else {
        if inner.height >= 4 {
            body.push(Line::default(), None);
        }
        let width = inner.width as usize;
        let available = inner.height.saturating_sub(body.line_count() as u16) as usize;
        let selected = palette.nav.selected.min(commands.len().saturating_sub(1));
        // Not the shared window: this one spends two of its rows on the
        // "N more" markers instead of scrolling silently.
        let (start, row_count) = palette_window(commands.len(), available, selected);
        if start > 0 {
            body.push(muted_line(&format!("  ↑ {start} more")), None);
        }
        push_row_window(
            &mut body,
            width,
            start..commands.len().min(start.saturating_add(row_count)),
            selected,
            |index, is_selected| {
                let command = commands[index];
                let marker = if is_selected { "> " } else { "  " };
                let shortcut = (inner.width >= 32 && !command.shortcut.is_empty())
                    .then_some(command.shortcut.as_str());
                let suffix_width = shortcut
                    .map(|shortcut| UnicodeWidthStr::width(shortcut).saturating_add(1))
                    .unwrap_or(0);
                let label_width = width
                    .saturating_sub(UnicodeWidthStr::width(marker))
                    .saturating_sub(suffix_width);
                let label = truncate_display(&command.label, label_width, Truncation::Right);
                // The shortcut keeps the right edge, so the gap is
                // whatever the label left over.
                let text = shortcut
                    .map(|shortcut| {
                        let gap = " ".repeat(
                            width
                                .saturating_sub(UnicodeWidthStr::width(marker))
                                .saturating_sub(UnicodeWidthStr::width(label.as_str()))
                                .saturating_sub(UnicodeWidthStr::width(shortcut)),
                        );
                        format!("{marker}{label}{gap}{shortcut}")
                    })
                    .unwrap_or_else(|| format!("{marker}{label}"));
                let style = if is_selected {
                    theme::selected()
                } else {
                    Style::default()
                };
                (text, style)
            },
        );
        let below = commands.len().saturating_sub(start + row_count);
        if below > 0 {
            body.push(muted_line(&format!("  ↓ {below} more")), None);
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
        body.push(error_line(error, inner.width as usize), None);
    } else if inner.height >= 6 {
        body.push(
            Line::styled(
                truncate_display(picker.model.name, inner.width as usize, Truncation::Right),
                Style::default().fg(MUTED),
            ),
            None,
        );
    }

    let width = inner.width as usize;
    let row_count = inner.height.saturating_sub(body.line_count() as u16) as usize;
    let choice_count = picker.choice_count();
    let selected = picker.nav.selected.min(choice_count.saturating_sub(1));
    push_row_window(
        &mut body,
        width,
        visible_rows(selected, choice_count, row_count),
        selected,
        |index, is_selected| {
            // Row 0 is the synthetic "let the provider decide" choice.
            let (id, name) = if index == 0 {
                ("default", "Provider default")
            } else {
                let variant = &picker.model.variants()[index - 1];
                (variant.id, variant.name)
            };
            let active = picker.active_variant.as_deref() == (index > 0).then_some(id);
            (
                marked_row(width, is_selected, active, name, &format!("  {id}")),
                choice_style(is_selected, active),
            )
        },
    );
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
        body.push(error_line(error, inner.width as usize), None);
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
    let width = inner.width as usize;
    push_row_window(
        &mut body,
        width,
        visible_rows(selected, choices.len(), row_count),
        selected,
        |index, is_selected| {
            let choice = choices[index];
            let active = choice == picker.active_theme;
            // The saved theme says so where the others show their id;
            // the cursor marker stays plain, since the row already
            // reads as active through its colour and that word.
            let suffix = if active {
                "  saved".to_string()
            } else if inner.width >= 34 {
                format!("  {}", choice.id())
            } else {
                String::new()
            };
            (
                marked_row(width, is_selected, false, choice.label(), &suffix),
                choice_style(is_selected, active),
            )
        },
    );
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
        // On a squeezed terminal the error takes the search line's row.
        if inner.height >= 3 {
            body.push(search_line, None);
        }
        body.push(error_line(error, inner.width as usize), None);
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
    let width = inner.width as usize;
    let row_count = inner.height.saturating_sub(body.line_count() as u16) as usize;
    if models.is_empty() && row_count > 0 {
        body.push(muted_line(" no matching models"), None);
    } else if row_count > 0 {
        let selected = picker.nav.selected.min(models.len().saturating_sub(1));
        push_row_window(
            &mut body,
            width,
            visible_rows(selected, models.len(), row_count),
            selected,
            |index, is_selected| {
                let model = models[index];
                let full_id = model.full_id();
                let active = full_id == picker.active_model;
                let marker = choice_marker(is_selected, active);
                // Narrow terminals drop the display name and the
                // context column; the id alone still identifies it.
                let text = if inner.width >= 50 {
                    let suffix = format!(
                        "  {full_id}  {}",
                        format_tokens_compact(model.context_limit)
                    );
                    // A column wider than `marked_row` reserves: the
                    // context number must not touch the border.
                    let name_width = width
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
                (text, choice_style(is_selected, active))
            },
        );
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

    /// One listing entry, the shape both resume surfaces are fed.
    fn summary(
        id: &str,
        title: Option<&str>,
        modified: std::time::SystemTime,
        cwd: Option<std::path::PathBuf>,
    ) -> ilar::session::SessionSummary {
        ilar::session::SessionSummary {
            id: id.into(),
            title: title.map(str::to_string),
            modified,
            cwd,
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
                cwd: None,
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

    /// Enter with nothing selected stays put in every picker: a query
    /// that matches nothing is a typo, and throwing the modal away
    /// costs the user the list they were browsing. Esc dismisses.
    #[test]
    fn empty_turn_picker_stays_on_enter() {
        let mut picker = TurnPicker::new(Vec::new());
        assert_eq!(
            picker.handle_key(KeyCode::Enter, false),
            TurnPickerAction::Stay
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

    /// Hovering a mapped modal row underlines its content — and only
    /// its content: the indent stays bare, headers and rows outside the
    /// area take nothing. The underline derives from the same hit map
    /// a click resolves through, so it cannot lie.
    #[test]
    fn hovering_a_mapped_modal_row_underlines_its_content() {
        use ratatui::buffer::Buffer;
        use ratatui::style::Modifier;

        let area = Rect::new(2, 1, 10, 3);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 14, 5));
        buffer.set_string(2, 2, "  item one", Style::default());
        let hit = ModalHit {
            area,
            rows: vec![None, Some(0), None],
        };
        assert!(
            !underline_hovered_item(&hit, &mut buffer, 3, 1),
            "a header row is not clickable, so it must not underline"
        );
        assert!(
            !underline_hovered_item(&hit, &mut buffer, 1, 2),
            "left of the area"
        );
        assert!(underline_hovered_item(&hit, &mut buffer, 3, 2));
        let underlined = |x: u16, y: u16| buffer[(x, y)].modifier.contains(Modifier::UNDERLINED);
        assert!(!underlined(2, 2), "leading blank stays bare");
        assert!(!underlined(3, 2), "leading blank stays bare");
        assert!(underlined(4, 2), "first content cell");
        assert!(underlined(8, 2), "an inner space underlines with the words");
        assert!(underlined(11, 2), "last content cell");
        assert!(!underlined(12, 2), "outside the area");
        assert!(
            !(2..12).any(|x| underlined(x, 1)),
            "the miss left the header row untouched"
        );
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

    /// Pasting into a filter is typing into it: the text lands in the
    /// query and the picker resets the way a keystroke would. Pastes
    /// used to vanish everywhere but the palette.
    #[test]
    fn pasting_appends_to_the_query_of_every_filterable_picker() {
        let now = std::time::SystemTime::now();
        let mut session = SessionPicker::new(
            vec![summary("aaa", Some("fix websearch fallback"), now, None)],
            None,
        );
        // Armed for deletion, then filtered: disarmed, like a keystroke.
        session.handle_key(KeyCode::Char('d'), true);
        session.insert_query("web");
        assert_eq!(session.query, "web");
        assert!(session.pending_delete.is_none());
        assert_eq!(
            session.handle_key(KeyCode::Char('d'), true),
            SessionPickerAction::Stay
        );

        let events = vec![meta_event(), user("u1", "first"), user("u2", "second")];
        let mut turn = TurnPicker::new(turn_entries(&events));
        turn.handle_key(KeyCode::Enter, false);
        turn.insert_query("sec");
        assert_eq!(turn.query, "sec");
        assert!(turn.armed.is_none(), "a filter paste must disarm");

        let mut link = LinkPicker::new(Vec::new());
        link.insert_query("docs");
        assert_eq!(link.query, "docs");

        let mut model = ModelPicker::new(ilar::model::catalog().iter().collect(), "none");
        model.error = Some("stale".into());
        model.nav.selected = 2;
        model.insert_query("gpt");
        assert_eq!(model.query, "gpt");
        assert!(model.filtered_models().len() < model.models.len());
        assert_eq!(model.selected_index(), 0);
        assert!(model.error.is_none());

        let mut theme = ThemePicker::new(theme::ThemeId::ALL[0]);
        assert!(matches!(
            theme.insert_query("gruv"),
            ThemePickerAction::Preview(_)
        ));
        assert_eq!(theme.query, "gruv");
        assert!(
            theme.selected_theme().id().contains("gruv"),
            "{}",
            theme.selected_theme().id()
        );

        let mut palette = CommandPalette::new(palette_items());
        palette.insert_query("theme");
        assert_eq!(palette.filtered_commands().len(), 1);
    }

    /// The content search is a typed grep query, so a pasted term must
    /// restart the scan exactly as a typed character does.
    #[test]
    fn pasting_into_the_session_search_restarts_the_scan() {
        let mut search = SessionSearch::new();
        search.handle_key(KeyCode::Char('a'), false);
        let generation = search.generation;
        search.push_rows(generation, vec![search_row("s1", "one", "ctx")]);

        assert_eq!(search.insert_query("uth"), SessionSearchAction::Rescan);
        assert_eq!(search.query, "auth");
        assert!(search.rows.is_empty(), "stale rows survived the paste");
        assert_ne!(search.generation, generation);
        assert!(search.scanning);

        // Nothing to add is not an edit: no rescan, no cleared rows.
        search.push_rows(search.generation, vec![search_row("s1", "one", "c")]);
        assert_eq!(search.insert_query("\n\t"), SessionSearchAction::Stay);
        assert_eq!(search.query, "auth");
        assert_eq!(search.rows.len(), 1);
    }

    /// The fuzzy scorer re-runs per render over every candidate, so a
    /// multi-megabyte clipboard must not become the query.
    #[test]
    fn a_pasted_novel_is_capped_to_a_filter_sized_query() {
        let mut link = LinkPicker::new(Vec::new());
        link.insert_query(&"x".repeat(1_000_000));
        assert_eq!(link.query.chars().count(), QUERY_CAP);

        // A query already at the cap absorbs nothing: no growth means
        // no reset hooks fire.
        let mut palette = CommandPalette::new(Vec::new());
        palette.insert_query(&"y".repeat(QUERY_CAP + 10));
        let before = palette.query.clone();
        palette.insert_query("z");
        assert_eq!(palette.query, before);
    }

    /// A single-line filter must survive a multi-line clipboard: the
    /// control characters are dropped rather than pushed into it.
    #[test]
    fn a_pasted_newline_never_reaches_a_single_line_query() {
        let mut link = LinkPicker::new(Vec::new());
        link.insert_query("one\ntwo\r\n\tthree\u{7}");
        assert_eq!(link.query, "onetwothree");

        // The one query that leaves the process: a stray newline would
        // ride into the cross-session grep.
        let mut search = SessionSearch::new();
        assert_eq!(
            search.insert_query("needle\nhere\n"),
            SessionSearchAction::Rescan
        );
        assert_eq!(search.query, "needlehere");

        // A theme paste with nothing in it previews where it stood
        // rather than re-ranking.
        let mut theme = ThemePicker::new(theme::ThemeId::ALL[0]);
        theme.move_selection(1);
        let standing = theme.selected_theme();
        assert_eq!(
            theme.insert_query("\n"),
            ThemePickerAction::Preview(standing)
        );
        assert_eq!(theme.query, "");

        let mut model = ModelPicker::new(ilar::model::catalog().iter().take(3).collect(), "none");
        model.nav.selected = 2;
        model.insert_query("\n\r\t");
        assert_eq!(model.query, "");
        assert_eq!(
            model.selected_index(),
            2,
            "a paste that changed nothing must not move the selection"
        );
    }

    /// A combining mark arrives as its own key event, so the query can
    /// hold multi-codepoint graphemes. Backspace must remove the whole
    /// grapheme in every query picker — the codepoint-popping pickers used
    /// to strand the base character.
    #[test]
    fn query_backspace_removes_whole_graphemes_in_every_picker() {
        let mut session = SessionPicker::new(Vec::new(), None);
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
            CommandPaletteAction::Choose(PaletteCommand::Session)
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
            CommandPaletteAction::Choose(PaletteCommand::Theme)
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
            // Undiscoverable otherwise: neither has a menu entry.
            "Ctrl-S",
            "Ctrl-L",
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
            summary("recent", Some("latest work"), now, None),
            summary(
                "older",
                None,
                now - std::time::Duration::from_secs(3_600),
                None,
            ),
        ];
        let mut picker = SessionPicker::new(sessions, None);
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

        let mut empty = SessionPicker::new(Vec::new(), None);
        assert_eq!(
            empty.handle_key(KeyCode::Enter, false),
            SessionPickerAction::Stay,
            "nothing to resume, but the modal is not the user's mistake"
        );
    }

    #[test]
    fn session_picker_fuzzy_filters_and_arms_deletion() {
        let now = std::time::SystemTime::now();
        let session = |id: &str, title: &str| summary(id, Some(title), now, None);
        let mut picker = SessionPicker::new(
            vec![
                session("aaa", "fix websearch fallback"),
                session("bbb", "voxel pagoda benchmark"),
                session("ccc", "readline editing chords"),
            ],
            None,
        );
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

    /// The stamp both resume surfaces put on every row. The reference
    /// time is an argument, so the buckets read the same on every
    /// machine and at every hour of the day.
    #[test]
    fn last_used_is_relative_within_a_day_and_a_date_beyond() {
        use chrono::TimeZone;
        let local = |year, month, day, hour| {
            std::time::SystemTime::from(
                chrono::Local
                    .with_ymd_and_hms(year, month, day, hour, 0, 0)
                    .single()
                    .expect("an unambiguous local time"),
            )
        };
        let now = local(2026, 8, 19, 12);
        let ago = |seconds: u64| now - std::time::Duration::from_secs(seconds);
        assert_eq!(last_used(ago(20), now), "just now");
        assert_eq!(last_used(ago(35 * 60), now), "35m ago");
        assert_eq!(last_used(ago(2 * 3_600), now), "2h ago");
        assert_eq!(last_used(ago(23 * 3_600), now), "23h ago");
        // Past a day the relative form stops meaning anything: a date,
        // with the year only when it is not this one.
        assert_eq!(last_used(local(2026, 8, 12, 9), now), "aug 12");
        assert_eq!(last_used(local(2024, 11, 3, 9), now), "2024-11-03");
        // Clock skew (a session written "in the future") must not panic.
        assert_eq!(
            last_used(now + std::time::Duration::from_secs(60), now),
            "just now"
        );
    }

    /// Where a session was last used, relative to where ilar runs now:
    /// only the same directory is "here", $HOME collapses to `~`, and a
    /// session that recorded no workspace is elsewhere with nothing to
    /// show for it.
    #[test]
    fn row_origin_marks_every_directory_but_this_one() {
        let path = std::path::Path::new;
        let home = path("/home/dev");
        let here = path("/home/dev/repos/ilar");
        assert_eq!(
            row_origin(Some(here), Some(here), Some(home)),
            RowOrigin::Here
        );
        assert_eq!(
            row_origin(Some(path("/home/dev/repos/foo")), Some(here), Some(home)),
            RowOrigin::Elsewhere(Some("~/repos/foo".into()))
        );
        // A subdirectory of this one is another directory, not this one.
        assert_eq!(
            row_origin(
                Some(path("/home/dev/repos/ilar/crates")),
                Some(here),
                Some(home)
            ),
            RowOrigin::Elsewhere(Some("~/repos/ilar/crates".into()))
        );
        assert_eq!(
            row_origin(Some(path("/srv/build")), Some(here), None),
            RowOrigin::Elsewhere(Some("/srv/build".into()))
        );
        // Older logs recorded nothing; they group with "elsewhere".
        assert_eq!(
            row_origin(None, Some(here), Some(home)),
            RowOrigin::Elsewhere(None)
        );
    }

    /// The acceptance case: the newest session belongs to another
    /// directory, so the top row — and the initial selection — is this
    /// directory's older one, stamped with when it was last used.
    #[test]
    fn the_session_picker_leads_with_this_directorys_last_session() {
        let here = std::path::PathBuf::from("/work/ilar");
        let there = std::path::PathBuf::from("/work/other");
        let now = std::time::SystemTime::now();
        let ago = |seconds: u64| now - std::time::Duration::from_secs(seconds);
        let mut picker = SessionPicker::new(
            vec![
                summary(
                    "newer",
                    Some("other project work"),
                    ago(600),
                    Some(there.clone()),
                ),
                summary("legacy", Some("no directory recorded"), ago(900), None),
                summary(
                    "older",
                    Some("work in this repo"),
                    ago(7_200),
                    Some(here.clone()),
                ),
            ],
            Some(here),
        );

        let (screen, _) = draw_modal(80, 20, |frame| render_session_picker(frame, &picker));
        let line_of = |needle: &str| {
            screen
                .lines()
                .position(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("{needle} is missing from:\n{screen}"))
        };
        let leading = line_of("work in this repo");
        assert!(leading < line_of("other project work"), "{screen}");
        assert!(leading < line_of("no directory recorded"), "{screen}");
        let row = screen.lines().nth(leading).unwrap();
        assert!(row.contains("> work in this repo"), "{row}");
        assert!(row.contains("2h ago"), "{row}");
        assert!(
            !row.contains('·'),
            "the directory ilar runs in needs no suffix: {row}"
        );
        // Rows from elsewhere name the directory they came from; one
        // that recorded none has nothing to name.
        assert!(
            screen
                .lines()
                .nth(line_of("other project work"))
                .unwrap()
                .contains("· /work/other"),
            "{screen}"
        );
        assert!(
            !screen
                .lines()
                .nth(line_of("no directory recorded"))
                .unwrap()
                .contains('·'),
            "{screen}"
        );
        // Enter resumes the row the picker opened on.
        assert_eq!(
            picker.handle_key(KeyCode::Enter, false),
            SessionPickerAction::Resume("older".into())
        );

        // A typed query is ordered by match quality, as it always was.
        for character in "project".chars() {
            picker.handle_key(KeyCode::Char(character), false);
        }
        assert_eq!(
            picker.handle_key(KeyCode::Enter, false),
            SessionPickerAction::Resume("newer".into())
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
        // No match: Enter has nothing to open, so the picker stays and
        // the query can be corrected.
        for character in "zzz".chars() {
            picker.handle_key(KeyCode::Char(character), false);
        }
        assert!(picker.filtered().is_empty());
        assert_eq!(picker.handle_key(KeyCode::Enter, false), PickerAction::Stay);
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
        assert_eq!(empty.handle_key(KeyCode::Enter, false), PickerAction::Stay);
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
        assert_eq!(empty.handle_key(KeyCode::Enter, false), PickerAction::Stay);
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

    /// The pending manager is fixed at 14 rows, so 20 pending items do
    /// not fit. The window has to follow the selection: rendering from
    /// item 0 armed deletes on rows nobody could see.
    #[test]
    fn a_scrolled_pending_manager_keeps_the_selection_on_screen() {
        let rows: Vec<String> = (0..20).map(|index| format!("item {index}")).collect();
        // 12 inner rows: the top of the list, the last row that fits
        // unscrolled, the first selection that has to scroll, and the
        // far end the arrow keys reach by wrapping.
        for selected in [0, 11, 12, 19] {
            let snapshot = PendingSnapshot {
                selected,
                armed: false,
                rows: rows.clone(),
            };
            let (screen, hit) =
                draw_modal(80, 24, |frame| render_pending_manager(frame, &snapshot));
            assert!(
                screen.contains(&format!("> item {selected} ")),
                "selection {selected} off-screen: {screen}"
            );
            assert!(
                hit.rows.contains(&Some(selected)),
                "selection {selected} unmapped: {:?}",
                hit.rows
            );
        }

        // The last item scrolls the window to items 8..=19, and the
        // armed marker rides along with it.
        let snapshot = PendingSnapshot {
            selected: 19,
            armed: true,
            rows,
        };
        let (screen, hit) = draw_modal(80, 24, |frame| render_pending_manager(frame, &snapshot));
        assert!(screen.contains("✗ item 19 "), "{screen}");
        assert!(!screen.contains("item 7 "), "{screen}");
        assert_eq!(hit.rows, (8..20).map(Some).collect::<Vec<_>>());
        assert_eq!(hit.item_at(hit.area.x, hit.area.y), Some(8));
        assert_eq!(hit.item_at(hit.area.x, hit.area.y + 11), Some(19));
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
        let session = |id: &str, title: &str| summary(id, Some(title), now, None);
        let mut picker = SessionPicker::new(
            vec![
                session("aaa", "fix websearch fallback"),
                session("bbb", "voxel pagoda benchmark"),
            ],
            None,
        );

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

    fn search_row(session: &str, title: &str, context_line: &str) -> SearchRow {
        SearchRow {
            session_id: session.into(),
            title: title.into(),
            event: 7,
            age: "3d".into(),
            origin: RowOrigin::default(),
            match_count: 1,
            title_match: false,
            context: vec![
                ("user".into(), "before the hit".into(), false),
                ("assistant".into(), context_line.into(), true),
            ],
        }
    }

    /// The same directory-first rule in the two-pane search: with no
    /// query the listing is a resume list, so this directory leads it —
    /// and typing hands the ordering back to the scan's match quality.
    #[test]
    fn the_session_search_listing_leads_with_this_directory() {
        let elsewhere = SearchRow {
            age: "10m ago".into(),
            origin: RowOrigin::Elsewhere(Some("~/repos/other".into())),
            ..search_row("newer", "other project", "ctx")
        };
        let here = SearchRow {
            age: "2h ago".into(),
            origin: RowOrigin::Here,
            ..search_row("older", "this repo", "ctx")
        };

        let mut listing = SessionSearch::new();
        listing.push_rows(0, vec![elsewhere.clone(), here.clone()]);
        assert_eq!(
            listing.selected().map(|row| row.session_id.as_str()),
            Some("older"),
            "the listing opened on another directory's session"
        );
        let (screen, _) = draw_modal(120, 24, |frame| render_session_search(frame, &listing));
        let line_of = |needle: &str| {
            screen
                .lines()
                .position(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("{needle} is missing from:\n{screen}"))
        };
        assert!(line_of("this repo") < line_of("other project"), "{screen}");
        assert!(screen.contains("2h ago"), "{screen}");
        assert!(screen.contains("~/repos/other"), "{screen}");

        // A typed query keeps the order the scan delivered.
        let mut typed = SessionSearch::new();
        typed.query = "needle".into();
        typed.push_rows(0, vec![elsewhere, here]);
        assert_eq!(
            typed.selected().map(|row| row.session_id.as_str()),
            Some("newer")
        );
    }

    /// The listing streams in while the user is already moving through
    /// it. Rows hoisted by a later batch must not slide the highlight
    /// onto a session the user never selected.
    #[test]
    fn a_batch_arriving_mid_scan_keeps_the_cursor_on_its_row() {
        let elsewhere = |id: &str| SearchRow {
            origin: RowOrigin::Elsewhere(Some("~/repos/other".into())),
            ..search_row(id, "other project", "ctx")
        };
        let mut search = SessionSearch::new();
        search.push_rows(0, vec![elsewhere("first"), elsewhere("second")]);
        search.move_selection(1);
        assert_eq!(
            search.selected().map(|row| row.session_id.as_str()),
            Some("second")
        );

        // A session from this directory arrives and leads the list; the
        // cursor stays on the row it was put on.
        search.push_rows(
            0,
            vec![SearchRow {
                origin: RowOrigin::Here,
                ..search_row("here", "this repo", "ctx")
            }],
        );
        assert_eq!(
            search.rows.first().map(|row| row.session_id.as_str()),
            Some("here")
        );
        assert_eq!(
            search.selected().map(|row| row.session_id.as_str()),
            Some("second")
        );

        // Untouched, the cursor is not a choice: it keeps the top row,
        // whichever session the scan turns out to lead with.
        let mut opening = SessionSearch::new();
        opening.push_rows(0, vec![elsewhere("first")]);
        opening.push_rows(
            0,
            vec![SearchRow {
                origin: RowOrigin::Here,
                ..search_row("here", "this repo", "ctx")
            }],
        );
        assert_eq!(
            opening.selected().map(|row| row.session_id.as_str()),
            Some("here")
        );
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
        search.push_rows(first_generation, vec![search_row("s1", "one", "ctx")]);
        assert_eq!(search.rows.len(), 1);

        // Another keystroke: rows clear, and the old scan's late
        // arrivals are dropped instead of mixing into the new list.
        assert_eq!(
            search.handle_key(KeyCode::Char('b'), false),
            SessionSearchAction::Rescan
        );
        assert!(search.rows.is_empty());
        search.push_rows(first_generation, vec![search_row("s1", "one", "ctx")]);
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
                search_row("s1", "one", "ctx"),
                search_row("s2", "two", "ctx"),
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
            .map(|_| search_row("s", "t", "ctx"))
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
                search_row("s1", "auth session", "the auth context"),
                search_row("s2", "parser session", "the parser context"),
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

    /// A session named for the query beats one that merely mentions
    /// it, however late its row streams in — stable within each group.
    #[test]
    fn a_title_match_outranks_a_content_match() {
        let mut search = SessionSearch::new();
        search.query = "parser".into();
        search.push_rows(
            0,
            vec![
                search_row("content-only", "auth work", "ctx"),
                SearchRow {
                    title_match: true,
                    ..search_row("named", "the parser rewrite", "ctx")
                },
            ],
        );
        assert_eq!(
            search
                .rows
                .iter()
                .map(|row| row.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["named", "content-only"]
        );
    }

    #[test]
    fn search_rows_carry_their_session_age() {
        let mut search = SessionSearch::new();
        search.query = "needle".into();
        search.push_rows(0, vec![search_row("s1", "auth work", "ctx")]);

        let (screen, _) = draw_modal(120, 24, |frame| render_session_search(frame, &search));
        assert!(screen.contains("3d"), "{screen}");
    }

    #[test]
    fn a_narrow_terminal_gets_the_list_alone() {
        let mut search = SessionSearch::new();
        search.query = "needle".into();
        search.push_rows(0, vec![search_row("s1", "one", "context text")]);

        let (screen, _) = draw_modal(60, 20, |frame| render_session_search(frame, &search));
        assert!(screen.contains("one"), "{screen}");
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
