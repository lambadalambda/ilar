//! The render pass: `App` state turned into a frame.
//!
//! Everything here reads `App` and draws. What it writes back is what
//! only the draw can know: where the rows landed (the hit maps a click
//! needs), the scroll and cache metrics the layout settles, and the
//! search matches, which are row indices. Real state changes — what the
//! session *is* — belong in app.rs.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, Padding, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState,
};
use unicode_width::UnicodeWidthStr;

use crate::app::App;
use crate::input::{input_accepts_keys, slash_candidates};
use crate::modals::{
    Modal, render_aside, render_command_palette, render_context_picker, render_help,
    render_link_picker, render_model_picker, render_pending_manager, render_session_picker,
    render_session_search, render_skill_picker, render_theme_picker, render_todos,
    render_turn_picker, render_variant_picker,
};
use crate::selection::{highlight_transcript_selection, selected_rows_unchanged, transcript_cells};
use crate::sidebar::{
    AgentPanel, ServicePanel, SidebarAction, agent_panel, carve_panel, carve_panel_capped,
    content_areas, lay_out_hits, render_todo_sidebar_snapshot, service_panel, todo_render_snapshot,
    todo_summary,
};
use crate::text::{
    Truncation, abbreviated_path, context_meter, context_usage, format_bytes, format_cost,
    format_tokens_compact, safe_lines, truncate_display, wrap_styled_line,
};
use crate::{Activity, ERROR, MAX_GOAL_ROUNDS, MUTED, theme};

const ASSISTANT: Color = theme::ASSISTANT;
const TOOL_ACTIVE: Color = theme::RUNNING;
const CONTENT_HORIZONTAL_PADDING: u16 = 2;

/// `$HOME`, read once. It does not change under a running process, and
/// the status line asked the environment for it on every frame.
fn home_dir() -> Option<&'static std::path::Path> {
    static HOME: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    HOME.get_or_init(|| std::env::var_os("HOME").map(std::path::PathBuf::from))
        .as_deref()
}
/// Show "no data Ns" in the status line once the stream has been silent
/// this long during thinking/responding.
const STREAM_STALL_AFTER: std::time::Duration = std::time::Duration::from_secs(3);

impl App {
    /// One truth for whether the mouse reaches the content behind the
    /// chrome: no modal in front, or Search — a transcript-reading mode
    /// whose clicks keep working underneath (main.rs routes mouse
    /// events by the same rule). Every hover underline outside a modal
    /// derives from this so the affordance cannot promise a click a
    /// modal would eat — or hide one it would not.
    pub(crate) fn mouse_reaches_content(&self) -> bool {
        matches!(self.active_modal(), None | Some(Modal::Search))
    }

    #[cfg(test)]
    pub(crate) fn transcript_lines(
        &self,
        width: u16,
        now: std::time::Instant,
    ) -> Vec<Line<'static>> {
        use crate::transcript::{transcript_entries, transcript_entry_rows};

        let mut output = Vec::new();
        for (index, entry) in transcript_entries(self.lines(), &self.expanded_tool_groups)
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

    /// The row a standing or transient notice gets, between the status
    /// line and the pending strip — so a reminder never hides the model,
    /// the usage or the meter. With no notice standing, a held backlog
    /// still says so: the pause used to have no indicator at all once
    /// its notice was cleared by an aborted turn.
    pub(crate) fn notice_line(&self, width: u16) -> Option<Line<'static>> {
        let width = width as usize;
        let (text, color) = match self.operational_notice() {
            Some((text, color)) => (text.to_string(), color),
            None if self.notifications_paused && !self.held_results.is_empty() => (
                format!(
                    "{} task result(s) held — {}",
                    self.held_results.len(),
                    crate::schedule::HELD_RESULT_ACTION
                ),
                theme::PRIMARY,
            ),
            None => return None,
        };
        let marker = match color {
            c if c == theme::ERROR => " × ",
            c if c == theme::WAITING => " ! ",
            _ => " · ",
        };
        Some(Line::from(vec![
            Span::styled(marker.to_string(), Style::default().fg(color)),
            Span::styled(
                truncate_display(&text, width.saturating_sub(3), Truncation::Right),
                Style::default().fg(color),
            ),
        ]))
    }

    pub(crate) fn status_line(&self, width: u16) -> Line<'static> {
        /// Room the activity's detail may take before the model and the
        /// meter start losing theirs.
        const STATUS_DETAIL_WIDTH: usize = 36;
        let width = width as usize;
        if self.search_active {
            // Nothing typed is not "no matches": there was no search yet.
            let counter = if self.search_query.is_empty() {
                "type to search".to_string()
            } else if self.search_matches.is_empty() {
                "no matches".to_string()
            } else {
                format!("{}/{}", self.search_current + 1, self.search_matches.len())
            };
            let hints = if width >= 70 {
                " · ↑↓ jump · Enter keep · Esc close"
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
        // `status` is the activity's detail — "running 3 tools",
        // "retrying provider (2/5)", "waiting for your answer" — set
        // beside every activity change. It replaces the bare word
        // when it says more; the word alone is the fallback.
        // The detail yields as the line narrows: the model, the usage
        // and the meter are what a narrow line is for, and a detail cut
        // to a few characters says nothing anyway.
        let detail_budget = width.saturating_sub(44).min(STATUS_DETAIL_WIDTH);
        let state = if self.status.is_empty()
            || self.status == state
            || self.status_activity != self.activity
            || detail_budget < 12
        {
            state.to_string()
        } else {
            truncate_display(&self.status, detail_budget, Truncation::Right)
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
            .unwrap_or_else(|| state.clone()),
            _ => state,
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
                "i{}/o~{} {} ctx {percent}",
                format_tokens_compact(latest.input_tokens),
                format_tokens_compact(out),
                Self::cache_hit_display(&latest)
            )),
            (Some(latest), None) => Some(format!(
                "i{}/o{} {} ctx {percent}",
                format_tokens_compact(latest.input_tokens),
                format_tokens_compact(latest.output_tokens),
                Self::cache_hit_display(&latest)
            )),
            (None, Some(out)) => Some(format!("o~{} ctx {percent}", format_tokens_compact(out))),
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
            // The whole bill: this session's own steps plus what its
            // subagents spent — the meter counts the children. The
            // tasks' share is named when it exists, so the number is
            // never mistaken for the root's own context spend.
            let all_tokens = |usage: &ilar::session::Usage| {
                usage.input_tokens
                    + usage.output_tokens
                    + usage.cache_read_input_tokens
                    + usage.cache_creation_input_tokens
            };
            let own = all_tokens(&self.session_usage);
            let tasks = all_tokens(&self.task_usage);
            let tokens = own + tasks;
            let cost = crate::session_view::add_costs(self.session_cost, self.task_cost);
            (tokens > 0).then(|| {
                let mut total = match cost {
                    Some(cost) => {
                        format!("Σ {} {}", format_tokens_compact(tokens), format_cost(cost))
                    }
                    None if ilar::model::plan_billed(&self.current_model) => {
                        format!("Σ {} plan", format_tokens_compact(tokens))
                    }
                    None => format!("Σ {}", format_tokens_compact(tokens)),
                };
                if tasks > 0 {
                    total.push_str(&format!(" (tasks {})", format_tokens_compact(tasks)));
                }
                total
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
                "in {} · out {out} · {} · {session}{context_display}",
                latest.input_tokens,
                Self::cache_hit_display(&latest)
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
        let cwd = show_cwd.then(|| abbreviated_path(&self.cwd, home_dir()));
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

    /// One row per message the model has not seen yet — steers first,
    /// since they deliver at the next step, then the turn-end queue —
    /// each stating when it will be sent, and a "+N more" tail past
    /// four.
    pub(crate) fn pending_strip_lines(&self, width: u16) -> Vec<Line<'static>> {
        /// Rows before the strip collapses into a "+N more" count.
        const SHOWN: usize = 4;
        let attachments: Vec<String> = self
            .pending_images
            .iter()
            .map(|image| {
                format!(
                    "image · {}",
                    crate::text::format_bytes(image.byte_len() as u64)
                )
            })
            .collect();
        // A waiting message says what rides with it: the strip has one
        // line per entry, so the attachments are counted here where the
        // transcript row lists them.
        let entries: Vec<(String, String)> = attachments
            .into_iter()
            .map(|label| ("attached · sends with your next message".to_string(), label))
            .chain(
                self.pending_steers
                    .iter()
                    .map(|message| pending_entry(message, "steering", "next step")),
            )
            .chain(
                self.queued_messages
                    .iter()
                    .map(|message| pending_entry(message, "queued", "when the turn ends")),
            )
            .collect();
        if entries.is_empty() {
            return Vec::new();
        }
        let mut lines = Vec::new();
        for (label, text) in entries.iter().take(SHOWN) {
            let lead = format!(" ↳ {label}: ");
            let body = truncate_display(
                text,
                (width as usize).saturating_sub(UnicodeWidthStr::width(lead.as_str())),
                Truncation::Right,
            );
            lines.push(Line::from(vec![
                Span::styled(lead, Style::default().fg(MUTED)),
                Span::styled(body, Style::default().fg(theme::USER)),
            ]));
        }
        if entries.len() > SHOWN {
            lines.push(Line::styled(
                format!("   +{} more waiting", entries.len() - SHOWN),
                Style::default().fg(MUTED),
            ));
        }
        lines
    }

    /// The focused child's transcript, drawn over the transcript area
    /// like the picker preview draws over the screen: read-only comes
    /// free, and the root transcript keeps rendering (and caching)
    /// underneath, ready for Esc.
    fn render_focus(&mut self, frame: &mut Frame, area: Rect) {
        let Some(focus) = self.focus.as_mut() else {
            return;
        };
        // Enter messages the agent on screen and Ctrl-G stops it, so
        // "read-only" was the one thing this view is not. A row that
        // cannot be messaged at all promises no Enter, and a finished
        // agent no cancel.
        let footer = match (crate::app::focus_can_send(focus), focus.running) {
            (false, true) => " ↑↓ scroll · ^G cancel ×2 · Esc close ",
            (false, false) => " agent finished · ↑↓ scroll · Esc close ",
            (true, true) => " ↑↓ scroll · Enter messages it · ^G cancel ×2 · Esc close ",
            (true, false) => " agent finished · ↑↓ scroll · Enter resumes it · Esc close ",
        };
        // The title says where in the timeline this is, so the rows
        // have to be laid out before the frame is drawn: the row count
        // is what `max_scroll` reads, and taking it from the previous
        // frame meant the first one said nothing and every one after
        // said where the view had been. A bordered rect's inside is
        // one cell in on each side — `Block::inner`, done by hand
        // because the block cannot be drawn yet.
        let inner_width = area.width.saturating_sub(2);
        let inner_height = area.height.saturating_sub(2);
        focus.cache.update(
            &focus.lines,
            &std::collections::HashSet::new(),
            focus.revision,
            inner_width,
            std::time::Instant::now(),
            focus.opened,
        );
        focus.content_rows = focus.cache.row_count();
        // The activity row takes a line of the viewport, not a line
        // past it: appended after the visible rows were trimmed, it
        // was clipped and the view said nothing after all.
        let activity_row = u16::from(focus.running);
        focus.viewport_rows = inner_height.saturating_sub(activity_row) as usize;
        let max_scroll = focus.max_scroll();
        if focus.follow_tail {
            focus.scroll_top = max_scroll;
        } else {
            focus.scroll_top = focus.scroll_top.min(max_scroll);
        }
        let scroll_label = if max_scroll == 0 {
            String::new()
        } else if focus.follow_tail {
            " · tail".to_string()
        } else {
            format!(" · {}%", focus.scroll_top.saturating_mul(100) / max_scroll)
        };
        let title = format!(
            " {}{scroll_label} ",
            truncate_display(
                &focus.title,
                (area.width as usize).saturating_sub(4 + scroll_label.len()),
                Truncation::Right
            )
        );
        let Some(inner) = crate::modals::modal_frame(frame, area, &title, TOOL_ACTIVE, footer)
        else {
            return;
        };
        // Laid out above, against the same width this rect has: the
        // cache machinery is the main transcript's, owned by the view.
        // No expanded groups — expansion clicks stay with the root.
        debug_assert_eq!(inner.width, inner_width);
        let rows = focus
            .cache
            .visible_rows(focus.scroll_top, focus.viewport_rows, &[]);
        let mut lines = rows.into_iter().map(|row| row.line).collect::<Vec<_>>();
        // A stalled child and a quiet one looked the same on the
        // surface built for watching them work. The same row the root
        // transcript ends with, from the same function.
        if focus.running
            && let Some(activity) = activity_line(
                true,
                Activity::Thinking,
                std::time::Instant::now(),
                focus.opened,
                None,
            )
        {
            lines.push(activity);
        }
        frame.render_widget(Paragraph::new(lines), inner);

        // And the scrollbar, on the same terms as the transcript's: a
        // view with nothing above or below it gets none.
        if max_scroll > 0 && area.height > 2 {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("│"))
                .thumb_symbol("┃");
            let mut state = ScrollbarState::new(max_scroll.saturating_add(1))
                .position(focus.scroll_top)
                .viewport_content_length(focus.viewport_rows);
            let bar = Rect::new(
                area.right().saturating_sub(2),
                area.y.saturating_add(1),
                1,
                area.height.saturating_sub(2),
            );
            frame.render_stateful_widget(scrollbar, bar, &mut state);
        }
    }

    pub(crate) fn render(&mut self, frame: &mut Frame) {
        // Refreshed below only if their panels actually draw; a stale
        // rect would take phantom clicks.
        self.sidebar_hits.clear();
        let input_width = frame.area().width.saturating_sub(2);
        let desired_input_height = self
            .input
            .visual_line_count(input_width)
            .saturating_add(2)
            .min(u16::MAX as usize) as u16;
        let input_height = input_height(desired_input_height, frame.area().height);
        let mut pending_lines = self.pending_strip_lines(frame.area().width);
        // The strip yields to the panes it sits between: on a cramped
        // terminal the transcript and input win.
        pending_lines
            .truncate(frame.area().height.saturating_sub(input_height + 6).min(5) as usize);
        let notice_line = self.notice_line(frame.area().width);
        let chunks = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(u16::from(notice_line.is_some())),
            Constraint::Length(pending_lines.len() as u16),
            Constraint::Length(input_height),
        ])
        .split(frame.area());
        let (notice_area, pending_area, input_chunk) = (chunks[2], chunks[3], chunks[4]);
        if let Some(line) = notice_line {
            frame.render_widget(Paragraph::new(line), notice_area);
        }
        if !pending_lines.is_empty() {
            frame.render_widget(Paragraph::new(pending_lines), pending_area);
        }

        let content_areas = content_areas(chunks[0]);
        let transcript_area = content_areas.transcript;
        let text_width = transcript_area
            .width
            .saturating_sub(2 + CONTENT_HORIZONTAL_PADDING * 2);
        let now = std::time::Instant::now();
        self.refresh_transcript_cache(text_width, now);
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
        // A blank spacer row only earns its place under something; on a
        // fresh session the activity line is all there is.
        if !activity_rows.is_empty() && !self.transcript_cache.is_empty() {
            activity_rows.insert(0, Line::default());
        }
        let viewport_rows = transcript_area.height.saturating_sub(2) as usize;
        let live_rows = self
            .transcript_cache
            .row_count()
            .saturating_add(activity_rows.len());
        // The session on offer, pinned above the live transcript in the
        // same pane. It is cut to what the pane can show beside the
        // conversation, keeping its header — which names what is on
        // offer — and the *end* of the tail, which is what resuming
        // would put in front of you. The header keeps its rows whatever
        // the transcript does: an offer that can still be answered must
        // not go invisible. So the offer never scrolls — it fits or it
        // is trimmed — and the transcript under it is where the scroll
        // still lives.
        let ghost_rows = match self.ghost.as_mut() {
            Some(ghost) => {
                let built = ghost.render(text_width, now);
                built.min(
                    viewport_rows
                        .saturating_sub(live_rows)
                        .max(ghost.header_rows()),
                )
            }
            None => 0,
        };
        let content_rows = live_rows.saturating_add(ghost_rows);
        self.update_scroll_metrics(content_rows, viewport_rows);
        // Streaming shifts row indices; keep search matches in sync with
        // the rows actually on screen — and the offer above them shifts
        // every one of them down while it is up.
        if self.search_active
            && self.search_computed_at != Some((self.transcript_revision, text_width, ghost_rows))
        {
            self.search_matches = self
                .transcript_cache
                .matching_rows(&self.search_query)
                .into_iter()
                .map(|row| row + ghost_rows)
                .collect();
            self.search_current = self
                .search_current
                .min(self.search_matches.len().saturating_sub(1));
            self.search_computed_at = Some((self.transcript_revision, text_width, ghost_rows));
        }
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
                // The session's own name, where a window title would be.
                Span::styled(
                    self.topic
                        .as_deref()
                        .map(|topic| {
                            format!(
                                " · {}",
                                truncate_display(
                                    topic,
                                    (transcript_area.width as usize).saturating_sub(24),
                                    Truncation::Right,
                                )
                            )
                        })
                        .unwrap_or_default(),
                    Style::default().fg(theme::SECONDARY),
                ),
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
        // The offer's rows first, then the transcript's: one pane, and
        // nothing in the offer is clickable — it is not the
        // conversation, so it has no rows to expand.
        let mut visible: Vec<crate::transcript::TranscriptRow> = self
            .ghost
            .as_ref()
            .map(|ghost| ghost.trimmed_to(ghost_rows))
            .unwrap_or_default()
            .into_iter()
            .map(|line| crate::transcript::TranscriptRow { line, target: None })
            .collect();
        let below = viewport_rows.saturating_sub(visible.len());
        if below > 0 {
            visible.extend(self.transcript_cache.visible_rows(
                self.scroll_top,
                below,
                &activity_rows,
            ));
        }
        self.transcript_hit_targets = visible.iter().map(|row| row.target.clone()).collect();
        // Hover marks what a click would hit right now — positional,
        // and off when a modal in front owns the mouse.
        let hover_row = if self.mouse_reaches_content() {
            self.hover.map(|point| point.row)
        } else {
            None
        };
        let text = visible
            .into_iter()
            .enumerate()
            .map(|(offset, row)| {
                let clickable = row.target.is_some();
                let mut line = row.line;
                if clickable && hover_row == Some(offset) {
                    crate::transcript::underline_content_spans(&mut line);
                }
                if self.search_active
                    && !self.search_query.is_empty()
                    && self
                        .search_matches
                        .binary_search(&(self.scroll_top + offset))
                        .is_ok()
                {
                    let current = self.search_matches.get(self.search_current)
                        == Some(&(self.scroll_top + offset));
                    // The current hit is bold on the same tint; themes
                    // without surfaces get reverse video back from
                    // `theme::apply`, which is what a search hit looks
                    // like on a canvas nobody can see.
                    for span in &mut line.spans {
                        span.style = span.style.bg(theme::SELECTION_BG);
                        if current {
                            span.style = span.style.add_modifier(Modifier::BOLD);
                        }
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
            // Not `content_rows`: ratatui scrolls until the last line
            // is at the *top* of the viewport, so it reads a content
            // length as one-past-the-last scroll position. We stop when
            // the last line reaches the bottom, and handing over the
            // row count left the thumb a viewport short of the track
            // end — the taller the terminal, the wider the gap.
            let mut state = ScrollbarState::new(max_scroll.saturating_add(1))
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

        // The focus view covers the transcript, not the sidebar: the
        // agents panel is the map of places you can go, and clicking
        // "main" on it is one of the ways back.
        self.render_focus(frame, transcript_area);

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
                if let Some(goal_area) = carve_panel(&mut todo_area, lines.len()) {
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
                            theme::title(theme::SECONDARY),
                        )));
                    frame.render_widget(Paragraph::new(lines).block(goal_block), goal_area);
                }
            }
            if self.agents_view.is_empty() {
                // An expansion is a decision about a roster; it does
                // not outlive it to pre-expand some future batch.
                self.agents_show_all = false;
            } else {
                let text_width = todo_area
                    .width
                    .saturating_sub(2 + CONTENT_HORIZONTAL_PADDING * 2)
                    .max(1) as usize;
                // Expansion drops the half-height cap: the click was an
                // explicit decision to spend the todo list's space. It
                // also un-sticks itself the moment everyone fits the
                // ordinary cap again.
                let collapsed_budget = (todo_area.height / 2).saturating_sub(2) as usize;
                if self.agents_show_all && self.agents_view.len() * 2 < collapsed_budget {
                    self.agents_show_all = false;
                }
                let cap = if self.agents_show_all {
                    todo_area.height
                } else {
                    todo_area.height / 2
                };
                let AgentPanel {
                    mut lines,
                    more_toggle,
                    row_hits,
                } = agent_panel(
                    &self.agents_view,
                    self.agents_show_all,
                    text_width,
                    cap.saturating_sub(2) as usize,
                );
                if let Some(agent_area) = carve_panel_capped(&mut todo_area, lines.len(), cap) {
                    // Each drawn row line becomes a screen rect the
                    // dispatcher can hit — the navigation surface the
                    // focus view opens from. An agent is two of them
                    // and one target, so the hover lights both.
                    lay_out_hits(
                        agent_area,
                        &mut lines,
                        more_toggle
                            .map(|index| (vec![index], SidebarAction::ToggleAgents))
                            .into_iter()
                            .chain(
                                row_hits.into_iter().map(|(indices, target)| {
                                    (indices, SidebarAction::Focus(target))
                                }),
                            ),
                        self.sidebar_hover(),
                        &mut self.sidebar_hits,
                    );
                    let agent_block = Block::default()
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
                            agent_panel_title(&self.agents_view),
                            theme::title(theme::SECONDARY),
                        )));
                    frame.render_widget(Paragraph::new(lines).block(agent_block), agent_area);
                }
            }
            if !self.services_view.is_empty() {
                let text_width = todo_area
                    .width
                    .saturating_sub(2 + CONTENT_HORIZONTAL_PADDING * 2)
                    .max(1) as usize;
                let ServicePanel {
                    mut lines,
                    exited_toggle,
                } = service_panel(&self.services_view, self.services_show_exited, text_width);
                if let Some(service_area) = carve_panel(&mut todo_area, lines.len()) {
                    // The disclosure line's screen row, for the mouse —
                    // and the hover underline, like every clickable.
                    lay_out_hits(
                        service_area,
                        &mut lines,
                        exited_toggle
                            .map(|index| (vec![index], SidebarAction::ToggleExitedServices)),
                        self.sidebar_hover(),
                        &mut self.sidebar_hits,
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
                            theme::title(theme::SECONDARY),
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
                    theme::title(theme::SECONDARY),
                )));
            let inner = todo_block.inner(todo_area);
            let snapshot = {
                let todos = self.todos.lock().unwrap();
                todo_render_snapshot(&todos, inner.height as usize)
            };
            let lines = render_todo_sidebar_snapshot(&snapshot, inner.width, inner.height);
            frame.render_widget(Paragraph::new(lines).block(todo_block), todo_area);
        }

        frame.render_widget(Paragraph::new(self.status_line(chunks[1].width)), chunks[1]);

        // A focus view is not a modal for the prompt: typing there
        // talks to the agent on screen. `--view` has no runtime behind
        // it, so the box stays for the layout's sake but promises
        // nothing.
        let input_focused = !self.read_only && input_accepts_keys(self.busy, self.has_modal());
        // Whether the focused agent can be messaged at all: a
        // grandchild's or another session's row takes no message, so
        // the prompt must not be titled as that agent's.
        let focus_send = self.focus.as_ref().map(crate::app::focus_can_send);
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
        let input_area = input_block.inner(input_chunk);
        let input_view = self
            .input
            .multiline_view(input_area.width, input_area.height);
        let mut input_title = if self.read_only {
            " read-only · q leaves ".into()
        } else if focus_send == Some(false) {
            " nothing to send here · Esc close ".into()
        } else if let Some(focus) = &self.focus {
            format!(" to {} ", focus.title)
        } else if input_view.line_count > 1 {
            format!(
                " input {}/{} ",
                input_view.cursor_line, input_view.line_count
            )
        } else {
            " input ".into()
        };
        // Steers and queued messages are listed in the strip directly
        // above, one row each; counting them again here was noise.
        if !self.input_stash.is_empty() {
            input_title = format!("{}· {} stashed ", input_title, self.input_stash.len());
        }
        if let Some((_, round)) = &self.goal {
            input_title = format!("{input_title}· goal {round}/{MAX_GOAL_ROUNDS} ");
        }
        // An offer standing over an empty prompt says what the next
        // keystroke does, muted, where the next keystroke goes. It is
        // drawn instead of the prompt's rows, which are empty anyway,
        // and the first character typed takes both it and the offer.
        // A modal in front takes the offer's keys away (`ghost_step`
        // refuses them), and the prompt box stays visible under a
        // centred one, so the hint would be promising a resume that
        // Enter does not perform.
        let prompt_hint =
            (self.ghost.is_some() && !self.read_only && self.focus.is_none() && !self.has_modal())
                .then(|| ghost_prompt_hint(input_area.width as usize))
                .flatten()
                .filter(|_| self.input.is_blank());
        let input_lines = match prompt_hint {
            Some(hint) => vec![Line::styled(hint, Style::default().fg(theme::MUTED))],
            None => input_view
                .lines
                .iter()
                .cloned()
                .map(Line::raw)
                .collect::<Vec<_>>(),
        };
        let mut input_block = input_block.title(Line::styled(
            input_title,
            theme::title(if input_focused {
                theme::USER
            } else {
                theme::SECONDARY
            }),
        ));
        // A footer is a promise about the next keystroke. `--view` can
        // keep none of them, and in a focus view the prompt belongs to
        // the agent on screen — the stash, the history and the images
        // are the root's, and Esc is the way back to them.
        // Only the keys this terminal can actually deliver: without an
        // extended-keys protocol Shift-Enter *is* Enter, and offering
        // it is how a sent draft looks like a swallowed keystroke.
        let newline = self.keys.newline_keys();
        // The thresholds below are tuned to the long form. The short
        // one is that much narrower, so it earns the difference back
        // rather than being suppressed at a width where it would fit.
        let room = input_chunk.width as usize + ("Shift-Enter/Ctrl-J".len() - newline.len());
        let input_help: Option<String> =
            if self.read_only || focus_send == Some(false) || prompt_hint.is_some() {
                // Under the offer's hint the footer's "Enter send" would
                // be a second, contradictory promise about the same key.
                None
            } else if self.focus.is_some() {
                Some(if room >= 70 {
                    format!(" Enter sends to the agent · {newline} newline · Esc close ")
                } else if input_chunk.width >= 48 {
                    " Enter sends to the agent · Esc close ".to_string()
                } else if input_chunk.width >= 28 {
                    " Enter sends · Esc close ".to_string()
                } else {
                    " Enter sends ".to_string()
                })
            } else if room >= 62 {
                Some(format!(" Enter send · {newline} newline · Ctrl-S stash "))
            } else if room >= 48 {
                Some(format!(" Enter send · {newline} newline "))
            } else {
                Some(" Enter send ".to_string())
            };
        if let Some(input_help) = input_help {
            input_block = input_block.title_bottom(
                Line::styled(input_help, Style::default().fg(theme::MUTED)).right_aligned(),
            );
        }
        let input = Paragraph::new(input_lines)
            .style(Style::default().fg(theme::PRIMARY))
            .block(input_block);
        frame.render_widget(input, input_chunk);

        // Inline slash-completion popup anchored above the input.
        // Building the inventory walks the command and skill lists and
        // allocates a pair of strings per entry — every frame, for an
        // input that cannot match anything unless it starts with `/`.
        let candidates = if self.input.text().starts_with('/') {
            slash_candidates(self.input.text(), &self.slash_inventory())
        } else {
            Vec::new()
        };
        // Not in a focus view: there the commands are not on offer —
        // ↑↓ scroll the transcript instead of moving the selection, Tab
        // does nothing, and Enter is refused. The popup promised a
        // completion nothing performed.
        if !candidates.is_empty() && !self.has_modal() && self.focus.is_none() {
            let rows = candidates.len().min(6) as u16;
            let height = rows + 2;
            let width = input_chunk.width.clamp(20, 64);
            let popup = Rect::new(
                input_chunk.x,
                input_chunk.y.saturating_sub(height),
                width,
                height.min(input_chunk.y),
            );
            if popup.height > 2 {
                frame.render_widget(Clear, popup);
                let block = Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme::focus_border())
                    .title(Line::styled(" commands ", theme::title(theme::MARKUP)))
                    .title_bottom(
                        Line::styled(
                            " ↑↓ · Tab/Enter complete ",
                            Style::default().fg(theme::MUTED),
                        )
                        .right_aligned(),
                    );
                let inner = block.inner(popup);
                frame.render_widget(block, popup);
                let selected = self.slash_selection().min(candidates.len() - 1);
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

        // Scraped every frame on purpose, though only a selection
        // consumes it. The invalidation check compares this frame's
        // cells against the previous frame's, and a selection is made
        // *between* frames — so gating the scrape on "a selection
        // exists" leaves the first such frame with nothing to compare
        // against, and output that changed in that window would be
        // copied from the new cells at the old coordinates.
        let transcript_cells = transcript_cells(frame.buffer_mut(), transcript_text_area);
        if self.transcript_selection.is_some_and(|selection| {
            self.transcript_text_area != transcript_text_area
                || !selected_rows_unchanged(&self.transcript_cells, &transcript_cells, selection)
        }) {
            self.clear_transcript_selection();
        }
        self.transcript_cells = transcript_cells;
        self.transcript_text_area = transcript_text_area;
        if let Some(selection) = self.transcript_selection {
            highlight_transcript_selection(
                frame.buffer_mut(),
                self.transcript_text_area,
                selection,
                &self.transcript_cells,
            );
        }

        // The caret marks where typing lands — the prompt, whether it
        // is the root's or a focused agent's. `--view` is the one place
        // typing lands nowhere, so it gets no caret.
        if input_focused && input_area.width > 0 && input_area.height > 0 {
            frame.set_cursor_position((
                input_area.x.saturating_add(input_view.cursor_x),
                input_area.y.saturating_add(input_view.cursor_y),
            ));
        }

        // Same precedence the key dispatcher uses, from the same value:
        // whatever is drawn on top is whatever is taking the keys. The
        // renderer hands back where its rows landed, so a click can be
        // mapped to the item it names.
        self.modal_hit = match self.active_modal() {
            Some(Modal::Question) => {
                self.question_modal
                    .as_ref()
                    .expect("question modal")
                    .render(frame, frame.area(), self.keys.newline_keys());
                None
            }
            Some(Modal::Grant) => {
                self.grant_modal
                    .as_ref()
                    .expect("grant modal")
                    .render(frame, frame.area());
                None
            }
            Some(Modal::Password) => {
                self.password_modal
                    .as_ref()
                    .expect("password modal")
                    .render(frame, frame.area());
                None
            }
            Some(Modal::PendingManager) => self
                .pending_snapshot()
                .map(|snapshot| render_pending_manager(frame, &snapshot)),
            Some(Modal::Help) => {
                render_help(frame, self.help_scroll, self.keys);
                None
            }
            Some(Modal::Todos) => {
                let todos = self.todos.lock().unwrap().clone();
                render_todos(frame, &todos, self.todos_scroll);
                None
            }
            Some(Modal::Aside) => {
                render_aside(frame, self.aside.as_ref().expect("aside modal"));
                None
            }
            Some(Modal::ThemePicker) => Some(render_theme_picker(
                frame,
                self.theme_picker.as_ref().expect("theme picker"),
            )),
            Some(Modal::SkillPicker) => Some(render_skill_picker(
                frame,
                self.skill_picker.as_ref().expect("skill picker"),
            )),
            Some(Modal::SessionPicker) => Some(render_session_picker(
                frame,
                self.session_picker.as_ref().expect("session picker"),
            )),
            Some(Modal::SessionSearch) => Some(render_session_search(
                frame,
                self.session_search.as_ref().expect("session search"),
            )),
            Some(Modal::TurnPicker) => Some(render_turn_picker(
                frame,
                self.turn_picker.as_ref().expect("turn picker"),
            )),
            Some(Modal::LinkPicker) => Some(render_link_picker(
                frame,
                self.link_picker.as_ref().expect("link picker"),
            )),
            Some(Modal::ModelPicker) => Some(render_model_picker(
                frame,
                self.model_picker.as_ref().expect("model picker"),
            )),
            Some(Modal::VariantPicker) => Some(render_variant_picker(
                frame,
                self.variant_picker.as_ref().expect("variant picker"),
            )),
            Some(Modal::ContextPicker) => Some(render_context_picker(
                frame,
                self.context_picker.as_ref().expect("context picker"),
            )),
            // Search renders into the status line, not an overlay.
            Some(Modal::Search) => None,
            Some(Modal::CommandPalette) => Some(render_command_palette(
                frame,
                self.command_palette.as_ref().expect("palette"),
            )),
            None => None,
        };
        // Hover marks what a click would select, exactly like the
        // transcript: the hit map *is* the clickability, so the
        // underline cannot lie about it.
        if let Some(hit) = &self.modal_hit
            && let Some((column, row)) = self.hover_screen
        {
            crate::modals::underline_hovered_item(hit, frame.buffer_mut(), column, row);
        }
        theme::apply(frame.buffer_mut(), self.theme);
    }
}

/// How tall the prompt may grow for what is in it. A big paste used to
/// take everything but four rows: the transcript collapsed to one line
/// and the pending strip truncated to nothing, so the screen became the
/// draft. Two fifths is the ceiling — the prompt scrolls inside it
/// (`InputBuffer::multiline_view` follows the cursor) — with a floor of
/// `SMALL_INPUT_ROWS`, so a handful of typed lines still fits whole on
/// a short terminal, and never more than the layout below can spare.
fn input_height(desired: u16, screen: u16) -> u16 {
    // Widened for the multiply: a debug build must not panic on a
    // terminal taller than 32767 rows, however absurd one would be.
    let ceiling = ((u32::from(screen) * 2 / 5) as u16)
        .max(SMALL_INPUT_ROWS)
        .min(screen.saturating_sub(4))
        .max(3);
    desired.clamp(3, ceiling)
}

/// A prompt this tall is nobody's runaway paste — four typed lines
/// between its borders — so the two-fifths ceiling never cuts below it.
const SMALL_INPUT_ROWS: u16 = 6;

/// What the empty prompt says while a session is on offer, at the width
/// it has to say it in. Resuming is the part worth keeping when the
/// prompt is narrow — typing is what happens anyway — and below room
/// for even that the hint stands down rather than trail off mid-word.
fn ghost_prompt_hint(width: usize) -> Option<&'static str> {
    const FULL: &str = "Enter resumes · type to start fresh";
    const SHORT: &str = "Enter resumes";
    if width >= FULL.chars().count() {
        Some(FULL)
    } else if width >= SHORT.chars().count() {
        Some(SHORT)
    } else {
        None
    }
}

/// The strip row a waiting message gets. A task or tool result is
/// mail, not something the user typed: it wears the collapsed headline
/// its transcript row will wear, under its own name, and never the
/// `<task-notification>` envelope. `kind` is the typed message's fate
/// ("steering", "queued"); `when` is shared, since a result waits the
/// same way words do.
fn pending_entry(message: &ilar::agent::Steer, kind: &str, when: &str) -> (String, String) {
    match crate::app::queued_result_headline(message) {
        Some(headline) => (format!("task result · {when}"), headline),
        None => (
            format!("{kind} · {when}"),
            crate::transcript::pending_summary(message),
        ),
    }
}

/// The agents panel's title. ⚙ background jobs and ✉ deliveries share
/// the panel with the agents but are not agents — a job has no session
/// to open and no model behind it — so the count says how many of each
/// rather than one number that disagrees with the word beside it.
fn agent_panel_title(rows: &[crate::sidebar::AgentRow]) -> String {
    let jobs = rows
        .iter()
        .filter(|row| row.agent == ilar::subagent::JOB_AGENT && !row.delivering)
        .count();
    let mail = rows.iter().filter(|row| row.delivering).count();
    let agents = rows.len() - jobs - mail;
    let mut title = format!("agents ({agents})");
    if jobs > 0 {
        title.push_str(&format!(" · {}", ilar::text::plural(jobs, "job")));
    }
    if mail > 0 {
        title.push_str(&format!(" · {mail} delivering"));
    }
    title
}

pub(crate) fn stream_liveness(
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

pub(crate) fn activity_line(
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
    use super::{ghost_prompt_hint, input_height};

    /// The offer's hint fits the prompt it is written in, or says the
    /// half that matters, or says nothing — never half a word.
    #[test]
    fn the_offers_hint_shortens_before_it_is_cut() {
        assert_eq!(
            ghost_prompt_hint(80),
            Some("Enter resumes · type to start fresh")
        );
        assert_eq!(
            ghost_prompt_hint(35),
            Some("Enter resumes · type to start fresh"),
            "the full hint at exactly its own width"
        );
        assert_eq!(ghost_prompt_hint(34), Some("Enter resumes"));
        assert_eq!(ghost_prompt_hint(13), Some("Enter resumes"));
        assert_eq!(ghost_prompt_hint(12), None);
        assert_eq!(ghost_prompt_hint(0), None);
    }

    /// A paste must not become the whole screen: the prompt takes at
    /// most two fifths and scrolls inside, leaving the transcript and
    /// the pending strip room to exist.
    #[test]
    fn a_big_draft_stops_at_two_fifths_of_the_screen() {
        // Small drafts are unaffected.
        assert_eq!(input_height(3, 40), 3);
        assert_eq!(input_height(8, 40), 8);
        // A 200-line paste yields to the transcript.
        assert_eq!(input_height(202, 40), 16);
        assert_eq!(input_height(202, 24), 9);
        // A few typed lines still fit whole on a short terminal.
        assert_eq!(input_height(5, 12), 5);
        assert_eq!(input_height(202, 12), 6);
        // Room for the pending strip's five rows and the status line
        // below the transcript, at the sizes a terminal actually is.
        for screen in [24u16, 40, 60, 100] {
            assert!(
                screen - input_height(u16::MAX, screen) >= 10,
                "{screen} leaves too little"
            );
        }
        // Tiny terminals keep a typable prompt rather than none.
        for screen in [0u16, 1, 3, 6] {
            assert_eq!(input_height(202, screen), 3, "{screen}");
        }
    }

    /// The panel's count said "agents" and counted ⚙ jobs and ✉
    /// deliveries too. A job has no session to open and no model behind
    /// it; each kind is named for what it is.
    #[test]
    fn the_agents_panel_counts_agents_and_names_the_rest() {
        use crate::sidebar::AgentRow;

        let row = |agent: &str, delivering: bool| AgentRow {
            session_id: "s".into(),
            depth: 0,
            description: "d".into(),
            agent: agent.into(),
            background: false,
            delivering,
            foreign_parent: None,
            elapsed: std::time::Duration::ZERO,
            waiting: false,
            quiet: None,
        };
        assert_eq!(super::agent_panel_title(&[]), "agents (0)");
        assert_eq!(
            super::agent_panel_title(&[row("build", false), row("explore", false)]),
            "agents (2)"
        );
        assert_eq!(
            super::agent_panel_title(&[
                row("build", false),
                row("job", false),
                row("job", false),
                row("explore", true),
            ]),
            "agents (1) · 2 jobs · 1 delivering"
        );
        assert_eq!(
            super::agent_panel_title(&[row("job", false)]),
            "agents (0) · 1 job"
        );
    }
}
