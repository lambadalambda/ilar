//! The transcript model and how one entry becomes rendered rows.
//!
//! `Line_` is the model: user turns, assistant text, thoughts, tool calls
//! and their nested subagent activity. Everything here turns that model
//! into styled rows for a given width, and applies loop events to it.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use ilar::agent::{LoopEvent, TurnOutcome};

use crate::text::{
    Truncation, bounded_detail, format_bytes, format_elapsed, safe_lines, safe_text,
    truncate_display, wrap_markdown_line, wrap_styled_line,
};
use crate::theme::{ERROR, MUTED, RUNNING as TOOL_ACTIVE};
use crate::{diff, markdown, theme};

/// A rendered line in the transcript.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)] // Tool entries own bounded detail and nested agent activity.
pub(crate) enum Line_ {
    User(String),
    Task {
        id: String,
        text: String,
        expanded: bool,
    },
    Job {
        id: String,
        text: String,
        expanded: bool,
    },
    Assistant(String),
    Thought {
        /// Click-target id; empty for nested subagent previews, which are
        /// not expandable.
        id: String,
        text: String,
        complete: bool,
        expanded: bool,
    },
    Tool {
        id: String,
        group_id: String,
        name: String,
        kind: ToolKind,
        arguments: String,
        argument_detail: String,
        diff: Vec<diff::DiffLine>,
        /// Live output tail while the tool runs (bash builds etc.).
        tail: String,
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
pub(crate) enum ToolState {
    Running,
    Complete,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ToolKind {
    Tool,
    Agent {
        name: String,
        /// Explicit per-task model override, shown next to the agent name.
        model: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolProgress {
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

/// Blank rows held below the transcript so the newest line has room to
/// breathe instead of sitting on the input box's border. Counted as
/// content, so following the tail actually shows them.
const TAIL_PADDING_ROWS: usize = 1;

/// Rendered rows, kept across frames so a streaming delta re-renders
/// only what it touched.
///
/// Change detection is by *mark*, not by comparison: the model is far
/// too large to clone and diff per token, so whoever mutates it says
/// which line index it started at (`mark_dirty_from`). Marks are only
/// trusted while they account for every revision bump since the last
/// update — an unmarked bump breaks the chain and the next update
/// rebuilds everything. A missed mark therefore costs work, never
/// correctness.
#[derive(Default)]
pub(crate) struct TranscriptRenderCache {
    width: Option<u16>,
    /// Revision of the last mark or update; marks must chain from it.
    revision: Option<u64>,
    /// Lowest line index whose rows may be stale as of `revision`.
    /// `usize::MAX` means nothing has changed since the last update.
    dirty_from: usize,
    /// The query `entries[..].matches` were scanned for.
    query: Option<String>,
    entries: Vec<CachedTranscriptEntry>,
    #[cfg(test)]
    pub(crate) rebuilds: usize,
    /// Rows `matching_rows` has lowercased and scanned, ever.
    #[cfg(test)]
    pub(crate) searched_rows: usize,
}

struct CachedTranscriptEntry {
    /// The `lines` range this entry renders, so a mark can name the
    /// entries it invalidates.
    range: std::ops::Range<usize>,
    /// Group identity for a run of tool calls; `None` for a lone line.
    group: Option<CachedGroup>,
    /// Spinners and elapsed times move without the model changing.
    animated: bool,
    rows: Vec<TranscriptRow>,
    /// Row offsets within `rows` matching the cache's query; `None`
    /// until scanned, which is what keeps search off untouched rows.
    matches: Option<Vec<usize>>,
}

struct CachedGroup {
    id: String,
    expanded: bool,
    child: bool,
}

impl CachedTranscriptEntry {
    /// Borrow the entry back out of the model — enough to re-render an
    /// animated row without rescanning the transcript for its bounds.
    fn borrow<'a>(&self, lines: &'a [Line_]) -> TranscriptEntry<'a> {
        match &self.group {
            None => TranscriptEntry::Item(&lines[self.range.start]),
            Some(group) => TranscriptEntry::ToolGroup {
                id: group.id.clone(),
                calls: &lines[self.range.clone()],
                expanded: group.expanded,
                child: group.child,
            },
        }
    }
}

/// One unit of the transcript as rendered: a line, or a run of adjacent
/// tool calls shown as one collapsible group. Borrowed from the model —
/// building these must stay cheap enough to do per streaming delta.
#[derive(Debug)]
pub(crate) enum TranscriptEntry<'a> {
    Item(&'a Line_),
    ToolGroup {
        id: String,
        calls: &'a [Line_],
        expanded: bool,
        child: bool,
    },
}

impl TranscriptEntry<'_> {
    pub(crate) fn is_child(&self) -> bool {
        matches!(self, Self::ToolGroup { child: true, .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TranscriptHitTarget {
    ToolGroup(String),
    Tool(String),
    Thought(String),
}

#[derive(Clone)]
pub(crate) struct TranscriptRow {
    pub(crate) line: Line<'static>,
    pub(crate) target: Option<TranscriptHitTarget>,
}

impl TranscriptRenderCache {
    /// Drop everything: rows rendered at one width say nothing about
    /// another. Line edits do not need this — they mark instead.
    fn invalidate(&mut self) {
        self.revision = None;
        self.dirty_from = 0;
        self.entries.clear();
        self.query = None;
    }

    /// Record that `from` is the lowest line index whose rows may have
    /// changed at `revision` (the value the revision counter now holds).
    /// Marks narrow the next rebuild; they never widen what is
    /// considered clean, and a bump that arrives unmarked resets to a
    /// full rebuild.
    pub(crate) fn mark_dirty_from(&mut self, from: usize, revision: u64) {
        self.dirty_from = if self.revision == Some(revision.wrapping_sub(1)) {
            self.dirty_from.min(from)
        } else {
            0
        };
        self.revision = Some(revision);
    }

    pub(crate) fn update(
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
            self.invalidate();
        }
        let dirty_from = if self.revision == Some(revision) {
            self.dirty_from
        } else {
            0
        };
        // Grouping restarts at the entry that owns the first dirty line,
        // so a run of tool calls that grew regroups as a whole.
        let mut resume = self
            .entries
            .iter()
            .position(|entry| entry.range.end > dirty_from)
            .unwrap_or(self.entries.len());
        // A run of adjacent tool calls is one entry, so a call arriving
        // at a group's edge joins the run rather than starting a second
        // group beside it. A plain tool sitting where a group ends is
        // proof the grouping is stale — the group swallows anything it
        // can, so it could not have been there last time. One step back
        // is enough: two tool groups are never adjacent.
        if resume > 0
            && self.entries[resume - 1].group.is_some()
            && matches!(
                lines.get(self.entries[resume - 1].range.end),
                Some(Line_::Tool {
                    kind: ToolKind::Tool,
                    ..
                })
            )
        {
            resume -= 1;
        }
        let mut line = self
            .entries
            .get(resume)
            .map(|entry| entry.range.start)
            .unwrap_or_else(|| self.entries.last().map_or(0, |entry| entry.range.end))
            .min(lines.len());
        self.entries.truncate(resume);
        while line < lines.len() {
            let (entry, next) = transcript_entry_at(lines, expanded_groups, line);
            let index = self.entries.len();
            let rows =
                spaced_entry_rows(&entry, index, expanded_groups, width, now, activity_started);
            self.entries.push(CachedTranscriptEntry {
                range: line..next,
                group: match &entry {
                    TranscriptEntry::Item(_) => None,
                    TranscriptEntry::ToolGroup {
                        id,
                        expanded,
                        child,
                        ..
                    } => Some(CachedGroup {
                        id: id.clone(),
                        expanded: *expanded,
                        child: *child,
                    }),
                },
                animated: transcript_entry_animated(&entry),
                rows,
                matches: None,
            });
            #[cfg(test)]
            {
                self.rebuilds += 1;
            }
            line = next;
        }
        // Rows kept from before the first dirty line still animate.
        for index in 0..resume {
            if !self.entries[index].animated {
                continue;
            }
            let entry = self.entries[index].borrow(lines);
            let rows =
                spaced_entry_rows(&entry, index, expanded_groups, width, now, activity_started);
            let cached = &mut self.entries[index];
            cached.rows = rows;
            cached.matches = None;
            #[cfg(test)]
            {
                self.rebuilds += 1;
            }
        }
        self.revision = Some(revision);
        self.dirty_from = usize::MAX;
    }

    pub(crate) fn row_count(&self) -> usize {
        self.entries
            .iter()
            .map(|entry| entry.rows.len())
            .sum::<usize>()
            + TAIL_PADDING_ROWS
    }

    /// Whether the transcript has said anything yet. `row_count` cannot
    /// answer this: it always counts [`TAIL_PADDING_ROWS`], so it is
    /// never zero and an emptiness test against it is always false.
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.iter().all(|entry| entry.rows.is_empty())
    }

    /// Absolute indices of rows whose text contains `query`
    /// (case-insensitive), in row order. Per-entry results are kept, so
    /// a streaming delta only rescans the entry it re-rendered.
    pub(crate) fn matching_rows(&mut self, query: &str) -> Vec<usize> {
        if query.trim().is_empty() {
            self.query = None;
            return Vec::new();
        }
        if self.query.as_deref() != Some(query) {
            self.query = Some(query.to_string());
            for entry in &mut self.entries {
                entry.matches = None;
            }
        }
        let needle = query.to_lowercase();
        let mut matches = Vec::new();
        let mut base = 0usize;
        for entry in &mut self.entries {
            if entry.matches.is_none() {
                #[cfg(test)]
                {
                    self.searched_rows += entry.rows.len();
                }
                entry.matches = Some(matching_row_offsets(&entry.rows, &needle));
            }
            matches.extend(entry.matches.iter().flatten().map(|offset| base + *offset));
            base += entry.rows.len();
        }
        matches
    }

    pub(crate) fn visible_rows(
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
        let padding = vec![
            TranscriptRow {
                line: Line::default(),
                target: None,
            };
            TAIL_PADDING_ROWS
        ];
        for rows in self
            .entries
            .iter()
            .map(|entry| entry.rows.as_slice())
            .chain(std::iter::once(trailing.as_slice()))
            .chain(std::iter::once(padding.as_slice()))
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

pub(crate) fn reasoning_summary_title(summary: &str) -> String {
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

/// Row offsets whose text contains an already-lowercased `needle`.
fn matching_row_offsets(rows: &[TranscriptRow], needle: &str) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .filter(|(_, row)| {
            let text: String = row
                .line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            text.to_lowercase().contains(needle)
        })
        .map(|(offset, _)| offset)
        .collect()
}

/// An entry's rows plus the blank row that spaces it from the entry
/// above.
fn spaced_entry_rows(
    entry: &TranscriptEntry,
    index: usize,
    expanded_groups: &std::collections::HashSet<String>,
    width: u16,
    now: std::time::Instant,
    activity_started: std::time::Instant,
) -> Vec<TranscriptRow> {
    let mut rows =
        transcript_entry_rows(entry, expanded_groups, width, now, activity_started, false);
    if index > 0 && !entry.is_child() {
        rows.insert(
            0,
            TranscriptRow {
                line: Line::default(),
                target: None,
            },
        );
    }
    rows
}

/// The entry beginning at `start`, and the index after it. Adjacent
/// plain tool calls collapse into one group; everything else is one
/// line, one entry.
fn transcript_entry_at<'a>(
    lines: &'a [Line_],
    expanded_groups: &std::collections::HashSet<String>,
    start: usize,
) -> (TranscriptEntry<'a>, usize) {
    let Line_::Tool {
        id: first_call_id,
        group_id,
        kind: ToolKind::Tool,
        ..
    } = &lines[start]
    else {
        return (TranscriptEntry::Item(&lines[start]), start + 1);
    };
    let mut end = start;
    while end < lines.len()
        && matches!(
            &lines[end],
            Line_::Tool {
                kind: ToolKind::Tool,
                ..
            }
        )
    {
        end += 1;
    }
    let id = format!("{group_id}:{first_call_id}");
    (
        TranscriptEntry::ToolGroup {
            expanded: expanded_groups.contains(&id),
            id,
            calls: &lines[start..end],
            child: start > 0 && matches!(lines[start - 1], Line_::Thought { .. }),
        },
        end,
    )
}

pub(crate) fn transcript_entries<'a>(
    lines: &'a [Line_],
    expanded_groups: &std::collections::HashSet<String>,
) -> Vec<TranscriptEntry<'a>> {
    let mut entries = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let (entry, next) = transcript_entry_at(lines, expanded_groups, index);
        entries.push(entry);
        index = next;
    }
    entries
}

/// A user message's display text: the words, then one marker line per
/// attached image (the payload itself never renders).
pub(crate) fn user_text_with_images(text: &str, images: &[ilar::session::ImageContent]) -> String {
    format!("{text}{}", ilar::image::attachment_markers(images))
}

/// The hover affordance: underline what a click on this row would
/// act on. Whitespace and box-drawing spans (indent, branch glyphs)
/// are structure, not content, and stay bare.
pub(crate) fn underline_content_spans(line: &mut Line<'static>) {
    for span in &mut line.spans {
        let structural = span
            .content
            .chars()
            .all(|c| c.is_whitespace() || ('\u{2500}'..='\u{257F}').contains(&c));
        if !structural {
            span.style = span
                .style
                .add_modifier(ratatui::style::Modifier::UNDERLINED);
        }
    }
}

pub(crate) fn toggle_tool_expansion(lines: &mut [Line_], id: &str) -> bool {
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

/// The newest row carrying this call id. Ids repeat across a long
/// transcript; an event is about the row that is still live, so every
/// lookup here runs newest-first.
fn newest_tool_index(lines: &[Line_], id: &str) -> Option<usize> {
    lines
        .iter()
        .rposition(|line| matches!(line, Line_::Tool { id: line_id, .. } if line_id == id))
}

fn newest_tool_index_where(
    lines: &[Line_],
    id: &str,
    live: impl Fn(ToolState) -> bool,
) -> Option<usize> {
    lines.iter().rposition(
        |line| matches!(line, Line_::Tool { id: line_id, state, .. } if line_id == id && live(*state)),
    )
}

fn newest_running_tool_index(lines: &[Line_], id: &str) -> Option<usize> {
    newest_tool_index_where(lines, id, |state| state == ToolState::Running)
}

/// The top-level index whose subtree owns this activity's parent call.
fn subagent_owner_index(
    lines: &[Line_],
    root_session_id: &str,
    activity: &ilar::subagent::SubagentActivity,
) -> Option<usize> {
    if activity.parent_session_id == root_session_id || root_session_id.is_empty() {
        return newest_tool_index(lines, &activity.parent_call_id);
    }
    (0..lines.len()).find(|index| {
        session_lines_for_call(
            std::slice::from_ref(&lines[*index]),
            &activity.parent_session_id,
            &activity.parent_call_id,
        )
    })
}

/// Whether any tool in this subtree hosts `session_id` and holds the
/// row for `call_id` — the read-only mirror of
/// `session_lines_for_call_mut`.
fn session_lines_for_call(lines: &[Line_], session_id: &str, call_id: &str) -> bool {
    lines.iter().any(|line| match line {
        Line_::Tool {
            child_session_id,
            child_lines,
            ..
        } => {
            (child_session_id.as_deref() == Some(session_id)
                && child_lines
                    .iter()
                    .any(|line| matches!(line, Line_::Tool { id, .. } if id == call_id)))
                || session_lines_for_call(child_lines, session_id, call_id)
        }
        _ => false,
    })
}

/// Fold a subagent's event into the child timeline under its parent
/// tool row. Returns the top-level line index that changed, or `None`
/// when the parent row has not arrived yet (the caller buffers).
pub(crate) fn apply_subagent_activity(
    lines: &mut [Line_],
    root_session_id: &str,
    activity: &ilar::subagent::SubagentActivity,
) -> Option<usize> {
    let index = subagent_owner_index(lines, root_session_id, activity)?;
    let direct = activity.parent_session_id == root_session_id || root_session_id.is_empty();
    let subtree = std::slice::from_mut(&mut lines[index]);
    // The list the parent call lives in: the top level itself when the
    // activity belongs to this session, otherwise the child timeline of
    // whichever nested agent hosts it.
    let owner = if direct {
        subtree
    } else {
        session_lines_for_call_mut(
            subtree,
            &activity.parent_session_id,
            &activity.parent_call_id,
        )?
        .as_mut_slice()
    };
    let call_index = newest_tool_index(owner, &activity.parent_call_id)?;
    let Line_::Tool {
        child_lines,
        child_group,
        child_running,
        child_session_id,
        ..
    } = &mut owner[call_index]
    else {
        return None;
    };
    *child_session_id = Some(activity.child_session_id.clone());
    *child_running = !matches!(activity.event, LoopEvent::TurnDone { .. });
    apply_child_loop_event(
        child_lines,
        child_group,
        &activity.parent_call_id,
        &activity.event,
    );
    Some(index)
}

// ---------------------------------------------------------------------
// Loop events applied to a transcript model. Both the session
// transcript (app.rs) and the nested timeline under an agent row match
// the same `LoopEvent`, so the edits themselves live here once — only
// the surrounding side effects (status text, notices, stream
// accounting) belong to the session. Each helper returns the lowest
// line index whose rendering changed, which is what the render cache
// needs to narrow its rebuild.
// ---------------------------------------------------------------------

/// Grow the last assistant reply, or start one.
pub(crate) fn append_text_delta(lines: &mut Vec<Line_>, text: &str) -> usize {
    match lines.last_mut() {
        Some(Line_::Assistant(current)) => current.push_str(text),
        _ => lines.push(Line_::Assistant(text.to_string())),
    }
    lines.len() - 1
}

/// Grow the open thought by `delta` — bounded to a tail, so a model
/// that reasons for an hour does not carry an hour of text — or open
/// one with `id` (empty for nested previews, which are not expandable).
pub(crate) fn append_thought_delta(
    lines: &mut Vec<Line_>,
    delta: &str,
    id: impl FnOnce() -> String,
) -> usize {
    match lines.last_mut() {
        Some(Line_::Thought {
            text,
            complete: false,
            ..
        }) => append_thought_tail(text, delta),
        _ => {
            let mut text = String::new();
            append_thought_tail(&mut text, delta);
            lines.push(Line_::Thought {
                id: id(),
                text,
                complete: false,
                expanded: false,
            });
        }
    }
    lines.len() - 1
}

/// Note that reasoning is under way without keeping the text: nested
/// previews show that a child is thinking, not what about.
fn open_placeholder_thought(lines: &mut Vec<Line_>, title: &str) -> usize {
    if !matches!(
        lines.last(),
        Some(Line_::Thought {
            complete: false,
            ..
        })
    ) {
        lines.push(Line_::Thought {
            id: String::new(),
            text: title.into(),
            complete: false,
            expanded: false,
        });
    }
    lines.len() - 1
}

/// Close the newest open thought: its phase ended.
pub(crate) fn complete_open_thought(lines: &mut [Line_]) -> Option<usize> {
    let index = lines.iter().rposition(|line| {
        matches!(
            line,
            Line_::Thought {
                complete: false,
                ..
            }
        )
    })?;
    if let Line_::Thought { complete, .. } = &mut lines[index] {
        *complete = true;
    }
    Some(index)
}

/// A fresh running tool row.
fn new_tool_row(id: &str, group_id: String, name: &str) -> Line_ {
    Line_::Tool {
        id: id.to_string(),
        group_id,
        name: name.to_string(),
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
    }
}

pub(crate) fn push_tool_row(
    lines: &mut Vec<Line_>,
    id: &str,
    group_id: String,
    name: &str,
) -> usize {
    lines.push(new_tool_row(id, group_id, name));
    lines.len() - 1
}

pub(crate) fn set_tool_arguments(lines: &mut [Line_], id: &str, arguments: &str) -> Option<usize> {
    let index = newest_tool_index(lines, id)?;
    let Line_::Tool {
        arguments: current, ..
    } = &mut lines[index]
    else {
        return None;
    };
    *current = arguments.to_string();
    Some(index)
}

pub(crate) fn note_tool_input_progress(
    lines: &mut [Line_],
    id: &str,
    received_bytes: u64,
    last_data: std::time::Instant,
) -> Option<usize> {
    let index = newest_running_tool_index(lines, id)?;
    let Line_::Tool { progress, .. } = &mut lines[index] else {
        return None;
    };
    // Bytes still arriving is the least of what a row can say: once it
    // is queued or executing, that is the news.
    if matches!(
        progress,
        ToolProgress::Queued | ToolProgress::Executing { .. }
    ) {
        return None;
    }
    *progress = ToolProgress::Receiving {
        received_bytes,
        last_data,
    };
    Some(index)
}

pub(crate) fn complete_tool_input(lines: &mut [Line_], id: &str, arguments: &str) -> Option<usize> {
    let index = newest_running_tool_index(lines, id)?;
    let Line_::Tool {
        name,
        progress,
        argument_detail,
        diff,
        ..
    } = &mut lines[index]
    else {
        return None;
    };
    *progress = ToolProgress::Queued;
    *argument_detail = bounded_detail(arguments);
    *diff = diff::tool_diff(name, arguments);
    Some(index)
}

pub(crate) fn configure_subagent_row(
    lines: &mut [Line_],
    id: &str,
    agent: &str,
    model: &Option<String>,
    description: &str,
) -> Option<usize> {
    let index = newest_tool_index(lines, id)?;
    let Line_::Tool {
        kind, arguments, ..
    } = &mut lines[index]
    else {
        return None;
    };
    *kind = ToolKind::Agent {
        name: agent.to_string(),
        model: model.clone(),
    };
    *arguments = description.to_string();
    Some(index)
}

pub(crate) fn start_tool_execution(
    lines: &mut [Line_],
    id: &str,
    received_bytes: u64,
    started: std::time::Instant,
) -> Option<usize> {
    let index = newest_running_tool_index(lines, id)?;
    let Line_::Tool { progress, .. } = &mut lines[index] else {
        return None;
    };
    *progress = ToolProgress::Executing {
        received_bytes,
        started,
    };
    Some(index)
}

pub(crate) fn complete_tool_execution(lines: &mut [Line_], id: &str) -> Option<usize> {
    let index = newest_running_tool_index(lines, id)?;
    let Line_::Tool {
        state, progress, ..
    } = &mut lines[index]
    else {
        return None;
    };
    *state = ToolState::Complete;
    *progress = ToolProgress::None;
    Some(index)
}

pub(crate) fn set_tool_tail(lines: &mut [Line_], id: &str, tail: &str) -> Option<usize> {
    let index = newest_running_tool_index(lines, id)?;
    let Line_::Tool { tail: current, .. } = &mut lines[index] else {
        return None;
    };
    *current = tail.to_string();
    Some(index)
}

/// Settle the newest still-open row for this call. `None` means the
/// result belongs to no row we have.
pub(crate) fn finish_tool_row(
    lines: &mut [Line_],
    id: &str,
    is_error: bool,
    result: &str,
    child_session_id: &Option<String>,
) -> Option<usize> {
    let index = newest_tool_index_where(lines, id, |state| {
        matches!(state, ToolState::Running | ToolState::Complete)
    })?;
    let Line_::Tool {
        state,
        progress,
        result: current,
        child_session_id: current_child,
        ..
    } = &mut lines[index]
    else {
        return None;
    };
    *state = if is_error {
        ToolState::Failed
    } else {
        ToolState::Succeeded
    };
    *progress = ToolProgress::None;
    *current = Some(bounded_detail(result));
    *current_child = child_session_id.clone();
    Some(index)
}

/// Streaming thoughts that will never finish: whatever arrived is a
/// fragment of a sentence, and keeping it would read as content.
pub(crate) fn prune_incomplete_thoughts(lines: &mut Vec<Line_>) -> Option<usize> {
    let first = lines.iter().position(|line| {
        matches!(
            line,
            Line_::Thought {
                complete: false,
                ..
            }
        )
    })?;
    lines.retain(|line| {
        !matches!(
            line,
            Line_::Thought {
                complete: false,
                ..
            }
        )
    });
    Some(first)
}

fn apply_child_loop_event(lines: &mut Vec<Line_>, group: &mut u64, scope: &str, event: &LoopEvent) {
    match event {
        // A parent's task_message, delivered — shown when the child
        // actually saw it, like the root's own steers.
        LoopEvent::Steered { text } => {
            lines.push(Line_::User(text.clone()));
        }
        LoopEvent::TextDelta(text) => {
            append_text_delta(lines, text);
        }
        LoopEvent::ThinkingDelta(_) => {
            open_placeholder_thought(lines, "reasoning");
        }
        LoopEvent::ReasoningSummaryDelta(summary) => {
            append_thought_delta(lines, summary, String::new);
        }
        LoopEvent::ReasoningSummaryCompleted => {
            complete_open_thought(lines);
        }
        LoopEvent::ToolStarted { id, name } => {
            push_tool_row(lines, id, format!("{scope}:{group}"), name);
        }
        LoopEvent::ToolArguments { id, arguments } => {
            set_tool_arguments(lines, id, arguments);
        }
        LoopEvent::ToolInputProgress {
            id,
            received_bytes,
            last_data,
        } => {
            note_tool_input_progress(lines, id, *received_bytes, *last_data);
        }
        LoopEvent::ToolInputComplete { id, arguments } => {
            complete_tool_input(lines, id, arguments);
        }
        LoopEvent::SubagentConfigured {
            id,
            description,
            agent,
            model,
        } => {
            configure_subagent_row(lines, id, agent, model, description);
        }
        LoopEvent::ToolExecutionStarted {
            id,
            received_bytes,
            started,
        } => {
            start_tool_execution(lines, id, *received_bytes, *started);
        }
        LoopEvent::ToolExecutionCompleted { id } => {
            complete_tool_execution(lines, id);
        }
        LoopEvent::ToolOutputTail { id, tail } => {
            set_tool_tail(lines, id, tail);
        }
        LoopEvent::ToolFinished {
            id,
            is_error,
            result,
            child_session_id,
            ..
        } => {
            finish_tool_row(lines, id, *is_error, result, child_session_id);
        }
        LoopEvent::StepComplete { .. } => *group = group.saturating_add(1),
        LoopEvent::TurnDone { outcome } => {
            prune_incomplete_thoughts(lines);
            if *outcome != TurnOutcome::Completed {
                mark_running_tools_failed(lines);
            }
        }
        LoopEvent::TurnStarted | LoopEvent::ProviderRetry { .. } | LoopEvent::Compacted { .. } => {}
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

pub(crate) fn transcript_entry_rows(
    entry: &TranscriptEntry,
    expanded_groups: &std::collections::HashSet<String>,
    width: u16,
    now: std::time::Instant,
    activity_started: std::time::Instant,
    nested: bool,
) -> Vec<TranscriptRow> {
    match entry {
        TranscriptEntry::Item(item) => match *item {
            tool @ Line_::Tool { .. } => tool_entry_rows(
                tool,
                expanded_groups,
                width,
                now,
                activity_started,
                0,
                None,
                0,
            ),
            item => {
                // Expandable thoughts get a click target on their header
                // row — and while collapsed, on every preview row and
                // the "click to expand" hint, since that is where the
                // eye (and the pointer) lands. Expanded, the body stays
                // bare so a stray click cannot collapse a wall of text.
                let (thought_target, whole_preview) = match item {
                    Line_::Thought { id, expanded, .. }
                    | Line_::Task { id, expanded, .. }
                    | Line_::Job { id, expanded, .. }
                        if !id.is_empty() =>
                    {
                        (Some(TranscriptHitTarget::Thought(id.clone())), !*expanded)
                    }
                    _ => (None, false),
                };
                let mut first_line = true;
                transcript_entry_lines(item, width, now, activity_started)
                    .into_iter()
                    .flat_map(|line| wrap_styled_line(line, width as usize))
                    .map(|line| {
                        let first = std::mem::take(&mut first_line);
                        let target = if first || whole_preview {
                            thought_target.clone()
                        } else {
                            None
                        };
                        TranscriptRow { line, target }
                    })
                    .collect()
            }
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
            // Siblings align to their widest name — the exact padding
            // alignment needs, not a fixed safe column.
            let name_column = visible
                .iter()
                .filter_map(|call| match call {
                    Line_::Tool { name, kind, .. } => {
                        Some(UnicodeWidthStr::width(display_name(name, kind).as_str()))
                    }
                    _ => None,
                })
                .max()
                .unwrap_or(0);
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
                    name_column,
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
        // A tool group that worked is scaffolding, not news: there is one
        // under every thought. Green on all of them is green that cannot
        // also mean "this one succeeded".
        (call_count(calls), "✓", MUTED)
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

#[allow(clippy::too_many_arguments)]
fn tool_entry_rows(
    entry: &Line_,
    expanded_groups: &std::collections::HashSet<String>,
    width: u16,
    now: std::time::Instant,
    activity_started: std::time::Instant,
    indent: usize,
    branch: Option<&str>,
    name_column: usize,
) -> Vec<TranscriptRow> {
    let indent = indent.min(width as usize);
    let Line_::Tool {
        id,
        name,
        kind,
        arguments,
        argument_detail,
        diff,
        tail,
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
        name_column,
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
        // A truncated block's "… more" row advances the expansion,
        // exactly like clicking the header again.
        let more = Some(TranscriptHitTarget::Tool(id.clone()));
        if diff.is_empty() {
            rows.extend(tool_detail_rows(
                "args",
                argument_detail,
                width,
                indent + 4,
                if *full { usize::MAX } else { 4 },
                false,
                more.clone(),
            ));
        } else {
            rows.extend(tool_diff_rows(
                diff,
                width,
                indent + 4,
                if *full { usize::MAX } else { 8 },
                more.clone(),
            ));
        }
        if *state == ToolState::Running && !tail.is_empty() {
            rows.extend(tool_detail_rows(
                "tail",
                tail,
                width,
                indent + 4,
                if *full { usize::MAX } else { 6 },
                false,
                more.clone(),
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
                more,
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

/// Last couple of lines of a streaming child reply — enough to see it is
/// alive without flooding the parent transcript; the full reply is one
/// expansion away (and the parent distills it anyway).
fn preview_tail(text: &str) -> String {
    const PREVIEW_LINES: usize = 2;
    const PREVIEW_CHARS: usize = 240;
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let tail_start = lines.len().saturating_sub(PREVIEW_LINES);
    let joined = lines[tail_start..].join("\n");
    let tail = ilar::text::tail_str(&joined, PREVIEW_CHARS);
    if tail_start > 0 || tail.len() < text.trim().len() {
        format!("… {tail}")
    } else {
        tail.to_string()
    }
}

fn agent_live_preview(lines: &[Line_]) -> Vec<Line_> {
    let mut preview = if let Some(Line_::Assistant(text)) = lines.last() {
        vec![Line_::Assistant(preview_tail(text))]
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

/// Carry a row's tint to the edge of its column. A band that stops at the
/// last character reads as a highlighter pen; one that reaches the margin
/// reads as a surface, which is the whole point of having one.
fn pad_background(
    mut line: Line<'static>,
    width: usize,
    background: Option<ratatui::style::Color>,
) -> Line<'static> {
    let Some(background) = background else {
        return line;
    };
    let padding = width.saturating_sub(line.width());
    if padding > 0 {
        line.spans.push(Span::styled(
            " ".repeat(padding),
            Style::default().bg(background),
        ));
    }
    line
}

fn labeled_rows(
    label: &str,
    mut content: Vec<Line<'static>>,
    layout: &DetailLayout,
    limit: usize,
    error: bool,
    more_target: Option<TranscriptHitTarget>,
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
    let last_index = content.len() - 1;
    let label_width = layout.label_width;
    let label_style = Style::default().fg(if error { ERROR } else { MUTED });
    content
        .into_iter()
        .enumerate()
        .map(|(index, mut line)| {
            // The "… more" row takes the click that reveals the rest.
            let target = (truncated && index == last_index)
                .then(|| more_target.clone())
                .flatten();
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
                target,
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
    more_target: Option<TranscriptHitTarget>,
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
    labeled_rows(label, content, &layout, limit, error, more_target)
}

fn tool_diff_rows(
    diff: &[diff::DiffLine],
    width: u16,
    indent: usize,
    limit: usize,
    more_target: Option<TranscriptHitTarget>,
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
            let (marker, color, background) = match line.kind {
                diff::DiffKind::Added => ("+", theme::SUCCESS, Some(theme::DIFF_ADD_BG)),
                diff::DiffKind::Removed => ("-", ERROR, Some(theme::DIFF_DEL_BG)),
                diff::DiffKind::Context => (" ", MUTED, None),
            };
            let mut style = Style::default().fg(color);
            if let Some(background) = background {
                style = style.bg(background);
            }
            wrap_styled_line(
                Line::from(Span::styled(
                    format!("{marker} {}", safe_text(&line.text)),
                    style,
                )),
                layout.content_width,
            )
            .into_iter()
            .map(|line| pad_background(line, layout.content_width, background))
            .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    labeled_rows("diff", content, &layout, limit, false, more_target)
}

pub(crate) fn transcript_entry_lines(
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
        Line_::Thought {
            id,
            text,
            complete,
            expanded,
        } => {
            let state = if *complete { "Thought" } else { "Thinking" };
            // Reasoning summaries lead with their headline (bold/heading);
            // raw streamed thinking is most useful tail-first — show the
            // line currently being written. Completed thoughts show their
            // lead either way.
            let summary_style = {
                let trimmed = text.trim_start();
                trimmed.starts_with("**") || trimmed.starts_with('#')
            };
            let title = if *complete || summary_style {
                reasoning_summary_title(text)
            } else {
                text.lines()
                    .rev()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .map(|line| line.to_string())
                    .unwrap_or_else(|| reasoning_summary_title(text))
            };
            let disclosure = match (id.is_empty(), expanded) {
                (true, _) => "+",
                (false, true) => "▾",
                (false, false) => "▸",
            };
            // The label carries the hue and the title carries the text:
            // these rows repeat more than any other, and a full line of
            // saturated colour on each one flattens the whole transcript.
            let label = format!("{disclosure} {state}: ");
            let title = truncate_display(
                &title,
                (width as usize).saturating_sub(label.chars().count()),
                Truncation::Right,
            );
            let mut output = vec![Line::from(vec![
                Span::styled(label, Style::default().fg(theme::REASONING)),
                Span::styled(title, Style::default().fg(theme::SECONDARY)),
            ])];
            if *expanded {
                for line in safe_lines(text) {
                    output.push(Line::from(vec![
                        Span::styled("  │ ", Style::default().fg(theme::REASONING)),
                        Span::styled(line, Style::default().fg(MUTED)),
                    ]));
                }
            }
            output
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
        Line_::Task { text, expanded, .. } => {
            notification_lines(text, *expanded, "task ", theme::REASONING, width)
        }
        Line_::Job { text, expanded, .. } => {
            notification_lines(text, *expanded, "job  ", theme::WAITING, width)
        }
        // Production tool rendering goes through `tool_entry_rows`,
        // which owns disclosure, grouping and child timelines;
        // `transcript_entry_rows` routes every tool line there before
        // this function is reached. The flat single-row form survives
        // for exhaustiveness and for the tests that render one row in
        // isolation.
        Line_::Tool {
            name,
            kind,
            arguments,
            state,
            progress,
            ..
        } => vec![tool_line_with_disclosure(
            name,
            kind,
            arguments,
            *state,
            width,
            now.saturating_duration_since(activity_started),
            *progress,
            now,
            false,
            false,
            0,
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

/// Render the transcript to shareable Markdown (palette: Export).
pub(crate) fn transcript_markdown(session_id: &str, lines: &[Line_]) -> String {
    let mut output = format!("# ilar session {session_id}\n");
    for line in lines {
        match line {
            Line_::User(text) => {
                output.push_str("\n## You\n\n");
                output.push_str(text);
                output.push('\n');
            }
            Line_::Assistant(text) => {
                output.push_str("\n## ilar\n\n");
                output.push_str(text);
                output.push('\n');
            }
            Line_::Thought { text, .. } => {
                output.push_str("\n**Thought:**\n\n");
                for line in text.lines() {
                    output.push_str("> ");
                    output.push_str(line);
                    output.push('\n');
                }
            }
            Line_::Tool {
                name,
                arguments,
                result,
                state,
                ..
            } => {
                let outcome = match state {
                    ToolState::Failed => " (failed)",
                    _ => "",
                };
                output.push_str(&format!("\n- `{name}` {arguments}{outcome}\n"));
                if let Some(result) = result {
                    output.push_str("\n```\n");
                    for line in result.lines().take(40) {
                        output.push_str(line);
                        output.push('\n');
                    }
                    if result.lines().count() > 40 {
                        output.push_str("… (truncated)\n");
                    }
                    output.push_str("```\n");
                }
            }
            Line_::Task { text, .. } | Line_::Job { text, .. } | Line_::System(text) => {
                output.push_str(&format!("\n*{}*\n", text.lines().next().unwrap_or("")));
            }
        }
    }
    output
}

/// Live thinking is kept as a bounded tail: enough to inspect what the
/// model is doing without letting 100KB+ reasoning bloat the transcript.
const MAX_THOUGHT_CHARS: usize = 64 * 1024;

fn append_thought_tail(text: &mut String, delta: &str) {
    text.push_str(delta);
    if text.len() > MAX_THOUGHT_CHARS {
        *text = format!("…{}", ilar::text::tail_str(text, MAX_THOUGHT_CHARS));
    }
}

/// The name a row shows: the tool's own, or `agent@model` when the
/// task carries an explicit model override.
fn display_name(tool_name: &str, kind: &ToolKind) -> String {
    match kind {
        ToolKind::Tool => tool_name.to_string(),
        ToolKind::Agent { name, model } => match model {
            Some(model) => format!("{name}@{}", model.split('/').next_back().unwrap_or(model)),
            None => name.clone(),
        },
    }
}

/// One tool row with no disclosure state — a test seam. Nothing in the
/// running TUI renders a tool line this way: `tool_entry_rows` owns the
/// real path, expansion and all.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn tool_line(
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
        name, kind, arguments, state, width, elapsed, progress, now, false, false, 0,
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
    name_column: usize,
) -> Line<'static> {
    let width = width as usize;
    let tool_name = name;
    let arguments = safe_text(arguments)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let name = display_name(tool_name, kind);
    let (label, label_color) = match kind {
        ToolKind::Tool => ("tool", theme::SECONDARY),
        ToolKind::Agent { .. } => ("agent", theme::REASONING),
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
    let name_limit = available_name.clamp(1, 20);
    let name = truncate_display(&name, name_limit, Truncation::Right);
    // Padded exactly to the widest sibling in the row's own group —
    // that is what alignment costs, and no more. A standalone row has
    // no siblings and no padding.
    let name_padding = if width >= 72 {
        name_column
            .min(name_limit)
            .saturating_sub(UnicodeWidthStr::width(name.as_str()))
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
/// Background-task/job notifications render like subagent rows: a
/// one-line headline with a disclosure, the body only when expanded.
fn notification_lines(
    text: &str,
    expanded: bool,
    label: &str,
    color: ratatui::style::Color,
    width: u16,
) -> Vec<Line<'static>> {
    let mut lines = safe_lines(text).into_iter();
    let headline = lines.next().unwrap_or_default();
    let body: Vec<String> = lines.collect();
    let disclosure = if body.is_empty() {
        "  "
    } else if expanded {
        "▾ "
    } else {
        "▸ "
    };
    let mut output = vec![Line::from(vec![
        Span::styled(label.to_string(), theme::title(color)),
        Span::styled(disclosure.to_string(), Style::default().fg(color)),
        Span::styled(
            truncate_display(
                &headline,
                (width as usize).saturating_sub(label.len() + 2),
                Truncation::Right,
            ),
            Style::default().fg(theme::PRIMARY),
        ),
    ])];
    if expanded {
        for line in body {
            output.push(Line::from(vec![
                Span::raw("     ".to_string()),
                Span::styled(line, Style::default().fg(MUTED)),
            ]));
        }
    } else if !body.is_empty() {
        output.push(Line::from(vec![
            Span::raw("     ".to_string()),
            Span::styled(
                format!("… {} more line(s) — click to expand", body.len()),
                Style::default().fg(MUTED),
            ),
        ]));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::tests::rendered_text;

    #[test]
    fn truncated_detail_rows_and_collapsed_thought_previews_take_clicks() {
        let now = std::time::Instant::now();
        let tool = Line_::Tool {
            id: "call-1".into(),
            group_id: "g".into(),
            name: "read".into(),
            kind: ToolKind::Tool,
            arguments: "x".into(),
            argument_detail: "{}".into(),
            diff: Vec::new(),
            tail: String::new(),
            result: Some(
                (0..40)
                    .map(|index| format!("line {index}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            state: ToolState::Succeeded,
            progress: ToolProgress::None,
            expanded: true,
            full: false,
            child_lines: Vec::new(),
            child_group: 0,
            child_running: false,
            child_session_id: None,
        };
        let mut expanded_groups = std::collections::HashSet::new();
        expanded_groups.insert("g:call-1".to_string());
        let entries = transcript_entries(std::slice::from_ref(&tool), &expanded_groups);
        let rows = transcript_entry_rows(&entries[0], &expanded_groups, 100, now, now, false);
        let more_row = rows
            .iter()
            .find(|row| rendered_text(&row.line).contains("… more"))
            .expect("a truncated result row");
        assert_eq!(
            more_row.target,
            Some(TranscriptHitTarget::Tool("call-1".into())),
            "clicking the more row must advance the expansion"
        );

        // A collapsed notification row: the headline and its "… N more
        // line(s) — click to expand" hint both take the click.
        let text = (0..8)
            .map(|index| format!("body {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let task = Line_::Task {
            id: "t1".into(),
            text: text.clone(),
            expanded: false,
        };
        let entries = transcript_entries(std::slice::from_ref(&task), &expanded_groups);
        let rows = transcript_entry_rows(&entries[0], &expanded_groups, 100, now, now, false);
        assert!(rows.len() > 1);
        assert!(
            rows.iter()
                .any(|row| rendered_text(&row.line).contains("click to expand"))
        );
        assert!(
            rows.iter()
                .all(|row| row.target == Some(TranscriptHitTarget::Thought("t1".into()))),
            "every collapsed-preview row is a click target"
        );

        // Expanded, only the header keeps it: a stray click on a wall
        // of text must not collapse it.
        let task = Line_::Task {
            id: "t1".into(),
            text,
            expanded: true,
        };
        let entries = transcript_entries(std::slice::from_ref(&task), &expanded_groups);
        let rows = transcript_entry_rows(&entries[0], &expanded_groups, 100, now, now, false);
        assert_eq!(
            rows[0].target,
            Some(TranscriptHitTarget::Thought("t1".into()))
        );
        assert!(rows[1..].iter().all(|row| row.target.is_none()));
    }

    #[test]
    fn hover_underline_marks_content_and_skips_structure() {
        let mut line = Line::from(vec![
            Span::raw("  └─ "),
            Span::raw("read"),
            Span::raw(" main.rs"),
        ]);
        underline_content_spans(&mut line);

        assert!(
            !line.spans[0]
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::UNDERLINED),
            "branch glyphs and indent stay bare"
        );
        assert!(
            line.spans[1]
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::UNDERLINED)
        );
        assert!(
            line.spans[2]
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::UNDERLINED)
        );
    }

    #[test]
    fn the_transcript_tail_keeps_a_blank_row_off_the_input() {
        let mut cache = TranscriptRenderCache::default();
        let lines = vec![Line_::Assistant("done".into())];
        let expanded = std::collections::HashSet::new();
        let now = std::time::Instant::now();
        cache.update(&lines, &expanded, 1, 40, now, now);

        let rows = cache.visible_rows(0, 10, &[]);
        let text = rows
            .iter()
            .map(|row| rendered_text(&row.line))
            .collect::<Vec<_>>();

        assert!(text.first().unwrap().contains("done"), "{text:?}");
        assert_eq!(
            text.last().map(String::as_str),
            Some(""),
            "the answer hugs the input box: {text:?}"
        );
        // The padding is real content, so scrolling can reach it.
        assert_eq!(cache.row_count(), rows.len());

        // It sits below the activity rows, not above them.
        let busy = cache.visible_rows(0, 10, &[Line::raw("thinking…")]);
        let busy_text = busy
            .iter()
            .map(|row| rendered_text(&row.line))
            .collect::<Vec<_>>();
        assert_eq!(
            busy_text.last().map(String::as_str),
            Some(""),
            "{busy_text:?}"
        );
        assert!(
            busy_text[busy_text.len() - 2].contains("thinking"),
            "{busy_text:?}"
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
        let limited = tool_diff_rows(&diff, 80, 4, 8, None);
        assert_eq!(limited.len(), 8);
        assert!(rendered_text(&limited.last().unwrap().line).contains("… more"));
        assert!(
            !limited
                .iter()
                .any(|row| rendered_text(&row.line).contains("added line 11"))
        );

        let full = tool_diff_rows(&diff, 80, 4, usize::MAX, None);
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

    fn finished_tool(id: &str, name: &str, arguments: &str) -> Line_ {
        Line_::Tool {
            id: id.into(),
            group_id: "g".into(),
            name: name.into(),
            kind: ToolKind::Tool,
            arguments: arguments.into(),
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
        }
    }

    /// Sibling rows in a group pad their names to the widest among
    /// them — the exact cost of alignment, not a fixed column.
    #[test]
    fn grouped_tool_rows_align_to_their_widest_sibling() {
        let calls = [
            finished_tool("t1", "bash", "cargo test"),
            finished_tool("t2", "webfetch", "https://x"),
        ];
        let group = TranscriptEntry::ToolGroup {
            id: "g".into(),
            calls: &calls,
            expanded: true,
            child: false,
        };

        let now = std::time::Instant::now();
        let rows = transcript_entry_rows(
            &group,
            &std::collections::HashSet::new(),
            120,
            now,
            now,
            false,
        );
        let rendered: Vec<String> = rows.iter().map(|row| rendered_text(&row.line)).collect();
        let bash = rendered.iter().find(|row| row.contains("bash")).unwrap();
        let fetch = rendered
            .iter()
            .find(|row| row.contains("webfetch"))
            .unwrap();

        // Both check marks land in the same column: bash is padded by
        // exactly the four characters webfetch is longer.
        assert_eq!(bash.find('✓'), fetch.find('✓'), "{bash:?} vs {fetch:?}");
        assert!(bash.contains("bash     ✓"), "{bash}");
        assert!(fetch.contains("webfetch ✓"), "{fetch}");
    }

    #[test]
    fn transcript_markdown_renders_all_line_kinds() {
        let lines = vec![
            Line_::User("fix the bug".into()),
            Line_::Thought {
                id: "thought:1".into(),
                text: "check the parser\nthen the lexer".into(),
                complete: true,
                expanded: false,
            },
            Line_::Tool {
                id: "t1".into(),
                group_id: "g".into(),
                name: "bash".into(),
                kind: ToolKind::Tool,
                arguments: "cargo test".into(),
                argument_detail: String::new(),
                diff: Vec::new(),
                tail: String::new(),
                result: Some("all green".into()),
                state: ToolState::Succeeded,
                progress: ToolProgress::None,
                expanded: false,
                full: false,
                child_lines: Vec::new(),
                child_group: 0,
                child_running: false,
                child_session_id: None,
            },
            Line_::Assistant("Fixed it.".into()),
            Line_::System("switched to zai/glm-5.3".into()),
        ];
        let markdown = transcript_markdown("abcd1234-rest", &lines);
        assert!(markdown.starts_with("# ilar session abcd1234-rest"));
        assert!(markdown.contains("## You\n\nfix the bug"), "{markdown}");
        assert!(markdown.contains("> check the parser\n> then the lexer"));
        assert!(markdown.contains("- `bash` cargo test\n"));
        assert!(markdown.contains("```\nall green\n```"));
        assert!(markdown.contains("## ilar\n\nFixed it."));
        assert!(markdown.contains("*switched to zai/glm-5.3*"));
    }

    #[test]
    fn agent_reply_previews_are_bounded_tails() {
        // Short replies pass through untouched.
        assert_eq!(preview_tail("done"), "done");
        // Long replies show only the last lines, marked truncated.
        let long: String = (0..30)
            .map(|index| format!("finding number {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let preview = preview_tail(&long);
        assert!(preview.starts_with("… "), "{preview}");
        assert!(preview.contains("finding number 29"), "{preview}");
        assert!(!preview.contains("finding number 5"), "{preview}");

        // A live child with a long reply previews bounded in the parent.
        let child = vec![Line_::Assistant(long.clone())];
        let previewed = agent_live_preview(&child);
        let Some(Line_::Assistant(text)) = previewed.first() else {
            panic!("assistant preview expected: {previewed:?}");
        };
        assert!(text.lines().count() <= 3, "{text}");
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
                id: String::new(),
                text: "Inspecting state".into(),
                complete: true,
                expanded: false,
            },
            80,
            now,
            now,
        );
        assert_eq!(thought[0].spans[0].style.fg, Some(theme::REASONING));
        assert_ne!(theme::REASONING, theme::WAITING);
    }

    /// Chrome that appears on every row must not be the loudest thing on
    /// the screen. The label keeps the hue because it is short; the title
    /// is text and reads as text.
    #[test]
    fn repeated_chrome_spends_its_colour_sparingly() {
        let now = std::time::Instant::now();
        let thought = transcript_entry_lines(
            &Line_::Thought {
                id: String::new(),
                text: "Inspecting state".into(),
                complete: true,
                expanded: false,
            },
            80,
            now,
            now,
        );
        assert!(rendered_text(&thought[0]).contains("Inspecting state"));
        assert_eq!(thought[0].spans[1].style.fg, Some(theme::SECONDARY));

        // A group of calls that all worked is scaffolding; one that failed
        // is not.
        let succeeded = tool_group_line(3, 0, 0, false, 80);
        assert!(
            succeeded
                .spans
                .iter()
                .all(|span| span.style.fg == Some(MUTED)),
            "{succeeded:?}"
        );
        let failed = tool_group_line(3, 0, 1, false, 80);
        assert!(
            failed.spans.iter().any(|span| span.style.fg == Some(ERROR)),
            "{failed:?}"
        );
    }

    /// A diff tint that stops at the last character reads as a highlighter
    /// pen; the band has to reach the margin.
    #[test]
    fn diff_tints_reach_the_margin() {
        let rows = tool_diff_rows(
            &[
                diff::DiffLine {
                    kind: diff::DiffKind::Added,
                    text: "let x = 1;".into(),
                },
                diff::DiffLine {
                    kind: diff::DiffKind::Context,
                    text: "unchanged".into(),
                },
            ],
            60,
            0,
            10,
            None,
        );
        let added = &rows[0].line;
        assert_eq!(
            added.spans.last().unwrap().style.bg,
            Some(theme::DIFF_ADD_BG)
        );
        let context = &rows[1].line;
        assert!(
            context.spans.iter().all(|span| span.style.bg.is_none()),
            "unchanged rows stay untinted: {context:?}"
        );
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
    fn thought_tails_are_bounded() {
        let mut text = String::new();
        append_thought_tail(&mut text, &"x".repeat(MAX_THOUGHT_CHARS + 500));
        assert!(text.len() <= MAX_THOUGHT_CHARS + '…'.len_utf8());
        assert!(text.starts_with('…'));
        // Multi-byte boundary safety.
        let mut unicode = String::new();
        append_thought_tail(&mut unicode, &"é".repeat(MAX_THOUGHT_CHARS));
        assert!(unicode.starts_with('…'));
        assert!(unicode.ends_with('é'));
    }

    /// A subagent's reasoning is the same unbounded stream the parent's
    /// is: without the cap a long-running child grows its Thought row
    /// without limit.
    #[test]
    fn child_reasoning_summaries_are_bounded_like_the_parents() {
        let mut lines = Vec::new();
        let mut group = 0u64;
        for _ in 0..4 {
            apply_child_loop_event(
                &mut lines,
                &mut group,
                "call-1",
                &LoopEvent::ReasoningSummaryDelta("x".repeat(MAX_THOUGHT_CHARS / 2)),
            );
        }

        let Some(Line_::Thought { text, .. }) = lines.last() else {
            panic!("a child thought row: {lines:?}");
        };
        assert!(
            text.len() <= MAX_THOUGHT_CHARS + '…'.len_utf8(),
            "child thought grew to {} bytes",
            text.len()
        );
    }

    /// Call ids repeat across a long child transcript; an event is about
    /// the row that is live, not the first one that ever wore the id.
    #[test]
    fn child_tool_events_land_on_the_newest_row_with_that_id() {
        let mut lines = vec![finished_tool("call-1", "read", "stale")];
        let mut group = 0u64;
        apply_child_loop_event(
            &mut lines,
            &mut group,
            "scope",
            &LoopEvent::ToolStarted {
                id: "call-1".into(),
                name: "read".into(),
            },
        );
        apply_child_loop_event(
            &mut lines,
            &mut group,
            "scope",
            &LoopEvent::ToolArguments {
                id: "call-1".into(),
                arguments: "live".into(),
            },
        );

        let arguments = lines
            .iter()
            .map(|line| match line {
                Line_::Tool { arguments, .. } => arguments.as_str(),
                _ => "",
            })
            .collect::<Vec<_>>();
        assert_eq!(arguments, ["stale", "live"], "{lines:?}");
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
}
