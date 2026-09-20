//! Loop decisions, separated from loop effects.
//!
//! `run_app` fuses "what should happen" with "make it happen" in every
//! match arm, so none of it can be observed without a terminal, a
//! provider and a session store. Everything here answers a question and
//! returns the answer; the caller acts.
//!
//! These are deliberately shaped to compose: they all read one
//! `LoopState` snapshot and return a value, so the eventual
//! `decide(event, state) -> Vec<Intent>` is an assembly of functions
//! that already exist rather than a rewrite of them.

use crate::NoticeLevel;
use crate::modals::Modal;

/// What the loop can see when deciding. A snapshot, so a decision
/// cannot accidentally depend on something it did not declare.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct LoopState {
    /// A turn is in flight (the handle, not the busy flag: they differ
    /// while a turn is being aborted).
    pub(crate) turn_running: bool,
    /// The overlay holding the keyboard, if any.
    pub(crate) modal: Option<Modal>,
    /// The prompt holds a draft.
    pub(crate) input_blank: bool,
    /// A synthetic or real event is already waiting to be handled;
    /// posting another would clobber it.
    pub(crate) pending_event: bool,
    /// Messages waiting for the turn to finish.
    pub(crate) queued: usize,
    /// A live steer channel for the running turn.
    pub(crate) steerable: bool,
    /// Notifications are held until the user says otherwise.
    pub(crate) notifications_paused: bool,
    /// A turn ended badly with its chain committed, so there is
    /// something for a resume to continue from.
    pub(crate) retry_available: bool,
    /// The Ctrl-X leader is armed, so the next key belongs to it — it
    /// is a modal that draws nothing.
    pub(crate) model_key_pending: bool,
}

impl LoopState {
    /// Whether it is safe to hand the UI a synthetic Enter: a picker or
    /// search bar would swallow it, a draft would be overwritten, and a
    /// real pending event must not be displaced.
    fn accepts_synthetic_submit(&self) -> bool {
        self.modal.is_none() && self.input_blank && !self.pending_event
    }
}

/// Something for the loop to do. Decisions return these; `run_app`
/// performs them in one place.
///
/// This is what replaces the synthetic-Enter trick, where a decision
/// posted a fake keypress and hoped the dispatcher was in a state that
/// would route it correctly. An intent says what to do rather than
/// impersonating the user doing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Intent {
    /// Send this as a new turn.
    StartTurn(String),
    /// Continue a failed turn from its persisted conversation state.
    ResumeTurn,
    /// Take the head of the queue and send it.
    SendQueued,
    /// Steer the running turn; falls back to the queue when the
    /// channel is gone, because the turn ending mid-submit must not
    /// lose the message.
    Steer(String),
    /// Ask a /btw question beside whatever is running.
    Aside(String),
    /// Hold until the running turn completes.
    Queue(String),
    /// Pasted text, routed to whichever surface owned the keyboard.
    PastePalette(String),
    PasteSearch(String),
    PasteQuestion(String),
    /// The filter of whichever picker owns the keyboard.
    PasteModalQuery(String),
    /// The sudo password prompt's field.
    PastePassword(String),
    PasteInput(String),
    /// Drop the goal, having finished or run out of rounds.
    ClearGoal,
    /// Advance the goal to this round.
    AdvanceGoal(u32),
    /// A line in the transcript.
    SystemLine(String),
    Notice(String, NoticeLevel),
    /// Bare `/context`: pick the session's context window from a list.
    OpenContextPicker,
    /// `/context <size>`: set it directly; `None` restores the model's.
    SetContextWindow(Option<u64>),
}

/// Where pasted text goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PasteTarget {
    Palette,
    Search,
    Question,
    /// The typed filter of a picker — the session search's grep query
    /// included, which is nothing but a typed query.
    ModalQuery,
    Input,
    /// The sudo password prompt's field. A sudo password comes out of
    /// a manager, so it arrives as a paste far more often than it is
    /// typed.
    Password,
    /// A modal with nowhere to put it.
    Discard,
}

pub(crate) fn paste_target(state: &LoopState) -> PasteTarget {
    match state.modal {
        Some(Modal::CommandPalette) => PasteTarget::Palette,
        Some(Modal::Search) => PasteTarget::Search,
        Some(Modal::Question) => PasteTarget::Question,
        // Anything that takes typed characters takes pasted ones: the
        // user filtering by hand and filtering by clipboard are the
        // same intent.
        Some(
            Modal::SessionSearch
            | Modal::SessionPicker
            | Modal::TurnPicker
            | Modal::LinkPicker
            | Modal::ModelPicker
            | Modal::ThemePicker,
        ) => PasteTarget::ModalQuery,
        // The sudo password is the one prompt a paste belongs in; the
        // grant prompt's letters are its four answers, and a paste must
        // not pick one of them.
        Some(Modal::Password) => PasteTarget::Password,
        // Spelled out rather than a wildcard: a new modal with a filter
        // must fail to compile here instead of silently swallowing
        // pastes, the way the pickers used to.
        Some(
            Modal::Grant
            | Modal::Help
            | Modal::Todos
            | Modal::Aside
            | Modal::PendingManager
            | Modal::SkillPicker
            | Modal::VariantPicker
            | Modal::ContextPicker,
        ) => PasteTarget::Discard,
        None => PasteTarget::Input,
    }
}

/// What submitting the prompt does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubmitTarget {
    /// No turn running: start one.
    StartTurn,
    /// A turn is running and can take it now.
    Steer,
    /// A turn is running but cannot be steered — a notification routed
    /// from another session has no steer channel.
    Queue,
}

pub(crate) fn submit_target(state: &LoopState, busy: bool) -> SubmitTarget {
    if !state.turn_running && !busy {
        SubmitTarget::StartTurn
    } else if state.steerable {
        SubmitTarget::Steer
    } else {
        SubmitTarget::Queue
    }
}

/// What to do with the queue when a turn ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum QueueStep {
    /// Nothing waiting.
    Idle,
    /// Send the head of the queue.
    Send,
    /// Hold: the UI is not in a state that can accept it.
    Hold(usize),
}

pub(crate) fn queue_step(state: &LoopState, completed: bool) -> QueueStep {
    if state.queued == 0 {
        return QueueStep::Idle;
    }
    if completed && state.accepts_synthetic_submit() {
        QueueStep::Send
    } else {
        QueueStep::Hold(state.queued)
    }
}

/// What goal mode does when a turn ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GoalStep {
    /// Not in goal mode, or not a moment to act.
    Idle,
    /// The model reported the sentinel.
    Achieved,
    /// Out of rounds without it.
    CapReached,
    /// Run another round.
    Continue(u32),
}

pub(crate) fn goal_step(
    state: &LoopState,
    completed: bool,
    round: Option<u32>,
    achieved: bool,
    max_rounds: u32,
) -> GoalStep {
    let Some(round) = round else {
        return GoalStep::Idle;
    };
    // A queued message outranks a goal round: the user asked for
    // something more recently than the goal did.
    if !completed || state.queued > 0 || !state.accepts_synthetic_submit() {
        return GoalStep::Idle;
    }
    if achieved {
        GoalStep::Achieved
    } else if round >= max_rounds {
        GoalStep::CapReached
    } else {
        GoalStep::Continue(round + 1)
    }
}

/// Everything to do when a root turn finishes: the goal round, then the
/// queue. Order matters — a goal continuation fills the prompt, which
/// is exactly the state that must stop the queue draining over it, so
/// the two are decided together rather than by two blocks that have to
/// remember to observe each other.
pub(crate) fn after_turn(
    state: &LoopState,
    completed: bool,
    goal: Option<(&str, u32)>,
    achieved: bool,
    max_rounds: u32,
) -> Vec<Intent> {
    let mut intents = Vec::new();
    let round = goal.map(|(_, round)| round);
    match goal_step(state, completed, round, achieved, max_rounds) {
        GoalStep::Idle => {}
        GoalStep::Achieved => {
            let message = format!("goal achieved after {} round(s)", round.unwrap_or(0).max(1));
            intents.push(Intent::ClearGoal);
            intents.push(Intent::SystemLine(message));
        }
        GoalStep::CapReached => {
            // Neither the cap's name nor the sentinel the model is
            // asked to say: both are ours, and a person reading this
            // wants to know it stopped and why.
            let message = format!("stopped after {max_rounds} rounds without reaching the goal");
            intents.push(Intent::ClearGoal);
            intents.push(Intent::SystemLine(message.clone()));
            intents.push(Intent::Notice(message, NoticeLevel::Warning));
        }
        GoalStep::Continue(next_round) => {
            let (goal, _) = goal.expect("a continuing round implies a goal");
            intents.push(Intent::AdvanceGoal(next_round));
            intents.push(Intent::StartTurn(crate::goal_continuation_prompt(
                goal, next_round,
            )));
        }
    }
    // No need to re-observe after the goal round: `goal_step` only
    // continues when the queue is empty, and `queue_step` is `Idle` on
    // an empty queue, so the two cannot both want the turn.
    match queue_step(state, completed) {
        QueueStep::Idle => {}
        QueueStep::Send => intents.push(Intent::SendQueued),
        QueueStep::Hold(count) => intents.push(Intent::Notice(
            format!("{count} queued message(s) held — Ctrl-Q to review"),
            NoticeLevel::Warning,
        )),
    }
    intents
}

/// The maintenance commands: they open or run something in the client
/// and take no arguments. Both the submit decision and `prepare_prompt`
/// match on this list, so neither can grow a command the other misses.
pub(crate) const MAINTENANCE_COMMANDS: [&str; 4] = ["compact", "rewind", "fork", "sessions"];

/// The one-line usage a maintenance command shows when handed arguments
/// it does not take. One text per command, so the two validation sites
/// cannot drift apart the way they had.
pub(crate) fn maintenance_usage(name: &str) -> String {
    match name {
        "fork" => "usage: /fork — Ctrl-Y in the /rewind picker forks at a turn".into(),
        _ => format!("usage: /{name}"),
    }
}

/// The same, for the aside command, which does take an argument.
pub(crate) const ASIDE_USAGE: &str = "usage: /btw <question>";

pub(crate) const CONTEXT_USAGE: &str = "usage: /context [default|32k|128k|200000|1m]";

/// A context window size as typed: a token count, or one with a `k`
/// (×1024) or `m` (×1024²) suffix, case-insensitive. Zero is refused —
/// it would make every turn compact — and so is anything that
/// overflows.
pub(crate) fn parse_context_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let (digits, factor) = match text.char_indices().last()? {
        (index, 'k' | 'K') => (&text[..index], 1024),
        (index, 'm' | 'M') => (&text[..index], 1024 * 1024),
        _ => (text, 1),
    };
    digits
        .parse::<u64>()
        .ok()?
        .checked_mul(factor)
        .filter(|size| *size > 0)
}

/// `/context` alone opens the picker; with a size it applies at once.
/// Allowed mid-turn: the override is read when the next turn spawns,
/// so there is nothing running to wait for.
fn context_command(size: &str) -> Intent {
    if size.is_empty() {
        Intent::OpenContextPicker
    } else if size.eq_ignore_ascii_case("default") {
        Intent::SetContextWindow(None)
    } else {
        match parse_context_size(size) {
            Some(limit) => Intent::SetContextWindow(Some(limit)),
            None => Intent::Notice(CONTEXT_USAGE.into(), NoticeLevel::Warning),
        }
    }
}

/// What a submitted prompt becomes. The decision (`submit_target`) and
/// the payload travel together, so a call site cannot route the text
/// one way while believing it decided another.
///
/// Attachments are not asked about: whatever is pending on the prompt
/// rides the message wherever it goes — a fresh turn, a steer or the
/// queue — so there is nothing here to refuse over.
pub(crate) fn submit(state: &LoopState, busy: bool, text: String) -> Vec<Intent> {
    // Maintenance commands must never become steering text for the model.
    // An aside is read-only and runs beside anything — mid-turn is
    // exactly when it is wanted, and it must never become steering
    // text for the model.
    if let Some(("btw", question)) = crate::parse_slash_invocation(&text) {
        if question.trim().is_empty() {
            return vec![Intent::Notice(ASIDE_USAGE.into(), NoticeLevel::Warning)];
        }
        return vec![Intent::Aside(question.to_string())];
    }
    // Like the aside, never steering text: a window size is nothing
    // the model can act on.
    if let Some(("context", size)) = crate::parse_slash_invocation(&text) {
        return vec![context_command(size)];
    }
    if let Some((name, args)) = crate::parse_slash_invocation(&text)
        && MAINTENANCE_COMMANDS.contains(&name)
    {
        if !args.is_empty() {
            return vec![Intent::Notice(
                maintenance_usage(name),
                NoticeLevel::Warning,
            )];
        }
        if state.turn_running || busy {
            return vec![Intent::Notice(wait_before(name), NoticeLevel::Warning)];
        }
    }
    let target = submit_target(state, busy);
    // Every other `/name` — a goal, a project command, a skill — is a
    // command too: it is armed, expanded or routed by `prepare_prompt`,
    // which only runs on the way into a turn. Steering sends the line
    // raw, so the model reads "/goal ship it" as prose and nothing is
    // armed. The queue is fine — it drains through `prepare_prompt` —
    // so only the steer refuses, and the text stays on the prompt for
    // the moment the turn ends.
    if target == SubmitTarget::Steer
        && let Some((name, _)) = crate::parse_slash_invocation(&text)
    {
        return vec![Intent::Notice(wait_before(name), NoticeLevel::Warning)];
    }
    match target {
        SubmitTarget::StartTurn => vec![Intent::StartTurn(text)],
        SubmitTarget::Steer => vec![Intent::Steer(text)],
        SubmitTarget::Queue => vec![Intent::Queue(text)],
    }
}

/// The one refusal every command shares when something is already
/// running. One text, so the maintenance commands and the rest cannot
/// say it two ways.
fn wait_before(name: &str) -> String {
    format!("wait for the current operation before /{name}")
}

/// Whether a submitted prompt was refused outright: nothing but
/// notices came back, so nothing was sent and the text the user typed
/// is theirs to keep. `prepare_prompt` already restores the input on
/// its own refusals; this is how the decision layer's refusals reach
/// the same place.
pub(crate) fn refused(intents: &[Intent]) -> bool {
    !intents.is_empty() && intents.iter().all(|i| matches!(i, Intent::Notice(..)))
}

/// What pasted text becomes. A modal with no text field returns
/// nothing: it has nowhere to put text, and falling through to the
/// prompt behind it would edit something the user cannot see.
pub(crate) fn paste(state: &LoopState, text: String) -> Vec<Intent> {
    match paste_target(state) {
        PasteTarget::Palette => vec![Intent::PastePalette(text)],
        PasteTarget::Search => vec![Intent::PasteSearch(text)],
        PasteTarget::Question => vec![Intent::PasteQuestion(text)],
        PasteTarget::ModalQuery => vec![Intent::PasteModalQuery(text)],
        PasteTarget::Password => vec![Intent::PastePassword(text)],
        PasteTarget::Input => vec![Intent::PasteInput(text)],
        PasteTarget::Discard => Vec::new(),
    }
}

/// Resume the failed turn, or say why not: a resume cannot displace a
/// running turn, cannot invent a failure to continue, and must not eat
/// an unsent draft. Every refusal answers the keypress — Ctrl-R on a
/// healthy session used to do nothing at all.
pub(crate) fn retry(state: &LoopState, busy: bool) -> Vec<Intent> {
    if state.turn_running || busy {
        // "Something", not "a turn": busy also covers a compaction and
        // the session restore, neither of which is one.
        return vec![Intent::Notice(
            "something is already running — Ctrl-R resumes a turn that ended badly".into(),
            NoticeLevel::Info,
        )];
    }
    if !state.retry_available {
        return vec![Intent::Notice(
            "nothing to resume — Ctrl-R continues a turn that failed or was aborted".into(),
            NoticeLevel::Info,
        )];
    }
    if !state.input_blank {
        return vec![Intent::Notice(
            "input has an unsent draft — send or clear it before resuming".into(),
            NoticeLevel::Warning,
        )];
    }
    vec![Intent::ResumeTurn]
}

/// Whether a retry decision dismisses the pending manager that raised
/// it. Only a resume does: the modal owns the keyboard and would sit
/// over the turn it just restarted. A warning leaves it open, because
/// the draft it complains about is cleared from behind it.
pub(crate) fn retry_dismisses_manager(intents: &[Intent]) -> bool {
    intents.iter().any(|i| matches!(i, Intent::ResumeTurn))
}

/// The root turn's stall watchdog verdict: what the loop should do
/// about the provider's silence. Pure — the caller measures, this only
/// judges — so every boundary is testable without a half-hour hang.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StallVerdict {
    /// Data is flowing, a tool is running, or nothing is being watched.
    Quiet,
    /// Silent past the warn threshold: surface it and ring once, but
    /// keep the turn running — the user may know the model is deep in
    /// something worth the wait.
    Warn { silent_secs: u64 },
    /// Silent past the abort threshold: cancel through the ordinary
    /// abort path, so the transcript and the resume machinery see a
    /// normal abort.
    Abort { silent_secs: u64 },
}

/// Judge the root turn's liveness.
///
/// `silence` is how long the turn has produced literally nothing —
/// `None` when no clock is running (no root turn, or one already
/// aborting or paused on a question). `tool_in_flight` holds the
/// verdict at `Quiet` however old the clock is: a long silent tool is
/// the tool's business, not the provider's — the child watchdog's
/// known false positive on silent tools, deliberately not copied.
pub(crate) fn stall_verdict(
    silence: Option<std::time::Duration>,
    tool_in_flight: bool,
    warn_after: std::time::Duration,
    abort_after: std::time::Duration,
) -> StallVerdict {
    let Some(silence) = silence else {
        return StallVerdict::Quiet;
    };
    if tool_in_flight || silence < warn_after {
        return StallVerdict::Quiet;
    }
    let silent_secs = silence.as_secs();
    if silence < abort_after {
        StallVerdict::Warn { silent_secs }
    } else {
        StallVerdict::Abort { silent_secs }
    }
}

/// Whether a same-session background completion may start a turn now.
/// An overlay owning the keyboard counts: a turn starting underneath a
/// picker or the search bar moves the transcript out from under the
/// user. Foreign completions are not gated here at all — their
/// delivery resumes another session and takes nothing of this one's.
/// What the warm-cache compaction needs to know about the session,
/// gathered by the app so the decision stays a pure function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CacheCompactCheck {
    pub(crate) enabled: bool,
    /// Already fired this idle episode: once is the budget.
    pub(crate) fired: bool,
    /// When the last provider request ended; `None` until one has.
    pub(crate) idle_since: Option<std::time::Instant>,
    pub(crate) ttl: std::time::Duration,
    pub(crate) margin: std::time::Duration,
    pub(crate) context_used: u64,
    pub(crate) context_floor: u64,
    /// Deliveries to other sessions in flight: their endings may start a
    /// turn here, which is the cache refresh itself.
    pub(crate) deliveries_in_flight: usize,
}

/// Whether to compact now, while the provider still holds the prefix:
/// enabled, not yet this episode, nothing running or about to, the
/// context big enough for a cold read to hurt, and the window about to
/// close. A queued message or a live delivery means real work is
/// coming, and that work refreshes the cache by itself.
pub(crate) fn cache_compact_due(
    state: &LoopState,
    check: &CacheCompactCheck,
    now: std::time::Instant,
) -> bool {
    if !check.enabled || check.fired || state.turn_running || state.modal.is_some() {
        return false;
    }
    if state.queued > 0 || check.deliveries_in_flight > 0 {
        return false;
    }
    if check.context_used < check.context_floor {
        return false;
    }
    let Some(idle_since) = check.idle_since else {
        return false;
    };
    now.duration_since(idle_since) >= check.ttl.saturating_sub(check.margin)
}

pub(crate) fn may_start_notification_turn(state: &LoopState) -> bool {
    !state.turn_running && !state.notifications_paused && state.modal.is_none()
}

/// What a keypress does to the offer of this directory's last session
/// — see meta/issues/a-bare-ilar-offers-the-last-session-here.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GhostStep {
    /// Enter on an empty prompt: open the session on offer, exactly as
    /// the picker's resume does.
    Resume,
    /// The offer goes: the first character of a fresh message was
    /// typed, or Esc said so, or a message is on its way and the
    /// session being spoken to is the fresh one.
    Dismiss,
    /// Nothing about the offer changes.
    Keep,
}

/// Enter on an empty prompt takes the offer; typing anything at all
/// leaves it behind, the character landing in the prompt as usual; and
/// every key that neither types nor answers — scrolling, a shortcut,
/// Backspace on nothing — leaves the offer alone. Anything that owns
/// the keyboard owns those keys too: a modal in front, or the Ctrl-X
/// leader waiting for its second press, whose Enter must not resume a
/// session. Shift-Enter is a newline, never a send, and Esc over a
/// draft clears the draft, so one press does one thing.
pub(crate) fn ghost_step(state: &LoopState, key: crossterm::event::KeyEvent) -> GhostStep {
    use crossterm::event::{KeyCode, KeyModifiers};
    if state.modal.is_some() || state.model_key_pending {
        return GhostStep::Keep;
    }
    match key.code {
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => GhostStep::Keep,
        KeyCode::Enter if state.input_blank => GhostStep::Resume,
        KeyCode::Enter => GhostStep::Dismiss,
        KeyCode::Esc if state.input_blank => GhostStep::Dismiss,
        // A character typed is the choice made — and only a character
        // that reaches the prompt, so Ctrl-P and every other chord
        // still open what they open over an offer that stays.
        _ if crate::input::types_a_character(&key) => GhostStep::Dismiss,
        _ => GhostStep::Keep,
    }
}

/// Nesting depth for each `(session_id, parent_session_id)` row, in
/// registry order: a row whose parent is also listed sits one level
/// under it, walked transitively; anyone else — a root's child, a
/// foreign tree's root — sits at 0. The registry can list one session
/// twice (a delivery row beside its turn row); the first occurrence
/// speaks for both. A cycle in the pairs would mean the registry lied
/// about ancestry; the walk refuses to revisit a session rather than
/// hang on the lie.
pub(crate) fn tree_depths(edges: &[(String, String)]) -> Vec<usize> {
    let mut first_occurrence = std::collections::HashMap::new();
    for (index, (session_id, _)) in edges.iter().enumerate() {
        first_occurrence.entry(session_id.as_str()).or_insert(index);
    }
    edges
        .iter()
        .map(|(session_id, _)| {
            let mut depth = 0;
            let mut visited = std::collections::HashSet::new();
            let mut current = session_id.as_str();
            while visited.insert(current) {
                let parent = edges[first_occurrence[current]].1.as_str();
                if !first_occurrence.contains_key(parent) || visited.contains(parent) {
                    break;
                }
                depth += 1;
                current = parent;
            }
            depth
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idle() -> LoopState {
        LoopState {
            input_blank: true,
            ..LoopState::default()
        }
    }

    /// Idle, with a failed turn behind it: what Ctrl-R is for.
    fn resumable() -> LoopState {
        LoopState {
            retry_available: true,
            ..idle()
        }
    }

    /// Every modal with a typed query takes a paste; the ones with no
    /// text field are the only ones that may swallow it.
    #[test]
    fn paste_reaches_every_modal_that_accepts_typed_characters() {
        for modal in [
            Modal::SessionSearch,
            Modal::SessionPicker,
            Modal::TurnPicker,
            Modal::LinkPicker,
            Modal::ModelPicker,
            Modal::ThemePicker,
        ] {
            let state = LoopState {
                modal: Some(modal),
                ..idle()
            };
            assert_eq!(paste_target(&state), PasteTarget::ModalQuery, "{modal:?}");
            assert_eq!(
                paste(&state, "needle".into()),
                vec![Intent::PasteModalQuery("needle".into())],
                "{modal:?}"
            );
        }
        for modal in [
            Modal::Grant,
            Modal::Help,
            Modal::Todos,
            Modal::Aside,
            Modal::PendingManager,
            Modal::SkillPicker,
            Modal::VariantPicker,
            Modal::ContextPicker,
        ] {
            let state = LoopState {
                modal: Some(modal),
                ..idle()
            };
            assert_eq!(paste_target(&state), PasteTarget::Discard, "{modal:?}");
            assert_eq!(paste(&state, "needle".into()), Vec::new(), "{modal:?}");
        }
    }

    /// A sudo password comes out of a manager, so its own prompt is the
    /// one place a paste must land; the grant prompt behind it takes
    /// none.
    #[test]
    fn the_sudo_password_prompt_takes_the_paste() {
        let asking = LoopState {
            modal: Some(Modal::Password),
            ..idle()
        };
        assert_eq!(paste_target(&asking), PasteTarget::Password);
        assert_eq!(
            paste(&asking, "s3cret".into()),
            vec![Intent::PastePassword("s3cret".into())]
        );
    }

    #[test]
    fn context_sizes_parse_plain_counts_and_binary_suffixes() {
        assert_eq!(parse_context_size("200000"), Some(200_000));
        assert_eq!(parse_context_size(" 128k "), Some(131_072));
        assert_eq!(parse_context_size("32K"), Some(32_768));
        assert_eq!(parse_context_size("1m"), Some(1_048_576));
        assert_eq!(parse_context_size("1M"), Some(1_048_576));
        for rejected in ["", "k", "0", "0k", "1.5m", "128kb", "lots", "default"] {
            assert_eq!(parse_context_size(rejected), None, "{rejected:?}");
        }
        // ×1024² on a count near u64::MAX must fail rather than wrap.
        assert_eq!(parse_context_size("18446744073709551615m"), None);
    }

    #[test]
    fn context_command_picks_sets_or_complains_and_never_waits_for_a_turn() {
        let running = LoopState {
            turn_running: true,
            steerable: true,
            ..idle()
        };
        assert_eq!(
            submit(&idle(), false, "/context".into()),
            vec![Intent::OpenContextPicker]
        );
        assert_eq!(
            submit(&running, true, "/context 128k".into()),
            vec![Intent::SetContextWindow(Some(131_072))]
        );
        assert_eq!(
            submit(&running, true, "/context DEFAULT".into()),
            vec![Intent::SetContextWindow(None)]
        );
        assert_eq!(
            submit(&idle(), false, "/context lots".into()),
            vec![Intent::Notice(CONTEXT_USAGE.into(), NoticeLevel::Warning)]
        );
    }

    #[test]
    fn paste_follows_whichever_surface_owns_the_keyboard() {
        assert_eq!(paste_target(&idle()), PasteTarget::Input);
        let searching = LoopState {
            modal: Some(Modal::Search),
            ..idle()
        };
        assert_eq!(paste_target(&searching), PasteTarget::Search);
        let palette = LoopState {
            modal: Some(Modal::CommandPalette),
            ..idle()
        };
        assert_eq!(paste_target(&palette), PasteTarget::Palette);
        let question = LoopState {
            modal: Some(Modal::Question),
            ..idle()
        };
        assert_eq!(paste_target(&question), PasteTarget::Question);
        // A filterable picker takes it as filter text; a text-less one
        // drops it rather than falling through to the prompt behind it.
        let picker = LoopState {
            modal: Some(Modal::ModelPicker),
            ..idle()
        };
        assert_eq!(paste_target(&picker), PasteTarget::ModalQuery);
        let help = LoopState {
            modal: Some(Modal::Help),
            ..idle()
        };
        assert_eq!(paste_target(&help), PasteTarget::Discard);
    }

    #[test]
    fn submitting_starts_steers_or_queues() {
        assert_eq!(submit_target(&idle(), false), SubmitTarget::StartTurn);
        let running = LoopState {
            turn_running: true,
            steerable: true,
            ..idle()
        };
        assert_eq!(submit_target(&running, true), SubmitTarget::Steer);
        // A routed notification turn has no steer channel.
        let routed = LoopState {
            turn_running: true,
            steerable: false,
            ..idle()
        };
        assert_eq!(submit_target(&routed, true), SubmitTarget::Queue);
        // Aborting: the handle is gone but busy lingers, and starting a
        // second turn there would race the first.
        let aborting = LoopState {
            turn_running: false,
            steerable: false,
            ..idle()
        };
        assert_eq!(submit_target(&aborting, true), SubmitTarget::Queue);
    }

    #[test]
    fn a_mid_turn_btw_becomes_an_aside_never_steering_text() {
        let running = LoopState {
            turn_running: true,
            steerable: true,
            ..idle()
        };

        // Steerable and running — a plain message would steer, but a
        // /btw runs beside the turn instead of talking into it.
        assert_eq!(
            submit(&running, true, "/btw which port was it?".into()),
            vec![Intent::Aside("which port was it?".into())]
        );
        assert_eq!(
            submit(&idle(), false, "/btw which port was it?".into()),
            vec![Intent::Aside("which port was it?".into())]
        );
        assert_eq!(
            submit(&running, true, "/btw".into()),
            vec![Intent::Notice(
                "usage: /btw <question>".into(),
                NoticeLevel::Warning,
            )]
        );
    }

    #[test]
    fn compact_command_never_steers_a_running_model() {
        let running = LoopState {
            turn_running: true,
            steerable: true,
            ..idle()
        };

        assert_eq!(
            submit(&running, true, "/compact".into()),
            vec![Intent::Notice(
                "wait for the current operation before /compact".into(),
                NoticeLevel::Warning,
            )]
        );
        assert_eq!(
            submit(&running, true, "/compact now".into()),
            vec![Intent::Notice(
                "usage: /compact".into(),
                NoticeLevel::Warning,
            )]
        );
    }

    /// One class per command kind: a goal, a project command, a skill
    /// and a plain unknown are all `prepare_prompt`'s business, and
    /// `prepare_prompt` only runs on the way into a turn. Steering
    /// would send the line to the model as prose.
    #[test]
    fn no_slash_command_is_ever_steered_at_the_model() {
        let running = LoopState {
            turn_running: true,
            steerable: true,
            ..idle()
        };
        for text in [
            "/goal ship the parser",
            "/goal",
            "/deploy staging",
            "/skill-name",
            "/unknown-thing with args",
        ] {
            let name = crate::parse_slash_invocation(text)
                .expect("a slash invocation")
                .0;
            assert_eq!(
                submit(&running, true, text.into()),
                vec![Intent::Notice(
                    format!("wait for the current operation before /{name}"),
                    NoticeLevel::Warning,
                )],
                "{text:?} must not steer"
            );
            // The same text queues unchanged when there is no steer
            // channel: the queue drains through `prepare_prompt`.
            let routed = LoopState {
                steerable: false,
                ..running
            };
            assert_eq!(
                submit(&routed, true, text.into()),
                vec![Intent::Queue(text.into())],
                "{text:?} must survive the queue"
            );
            // Idle, it is a turn like any other: `prepare_prompt`
            // arms, expands or refuses it there.
            assert_eq!(
                submit(&idle(), false, text.into()),
                vec![Intent::StartTurn(text.into())],
                "{text:?} must start a turn"
            );
        }
        // Not a command: prose that happens to open with a slash still
        // steers, and so does an absolute path.
        for text in ["/etc/passwd is odd", "/ leading space"] {
            assert_eq!(
                submit(&running, true, text.into()),
                vec![Intent::Steer(text.into())],
                "{text:?} is not a command"
            );
        }
    }

    /// A refusal is a refusal whichever command class raised it: the
    /// caller puts the text back on the prompt.
    #[test]
    fn a_refusal_is_recognisable_as_one() {
        let running = LoopState {
            turn_running: true,
            steerable: true,
            ..idle()
        };
        assert!(refused(&submit(&running, true, "/goal ship it".into())));
        assert!(refused(&submit(&running, true, "/compact".into())));
        assert!(refused(&submit(&idle(), false, "/context lots".into())));
        assert!(refused(&submit(&idle(), false, "/btw".into())));
        // Anything that actually went somewhere is not a refusal.
        assert!(!refused(&submit(&idle(), false, "hello".into())));
        assert!(!refused(&submit(&running, true, "hello".into())));
        assert!(!refused(&submit(&idle(), false, "/btw why?".into())));
        assert!(!refused(&submit(&idle(), false, "/context 128k".into())));
        assert!(!refused(&[]));
    }

    #[test]
    fn the_queue_only_drains_into_a_ui_that_can_take_it() {
        let waiting = LoopState {
            queued: 2,
            ..idle()
        };
        assert_eq!(queue_step(&waiting, true), QueueStep::Send);
        assert_eq!(queue_step(&idle(), true), QueueStep::Idle);
        // Not completed: aborted or errored turns hold the queue.
        assert_eq!(queue_step(&waiting, false), QueueStep::Hold(2));
        for blocker in [
            LoopState {
                modal: Some(Modal::Search),
                ..waiting
            },
            LoopState {
                input_blank: false,
                ..waiting
            },
            LoopState {
                pending_event: true,
                ..waiting
            },
        ] {
            assert_eq!(
                queue_step(&blocker, true),
                QueueStep::Hold(2),
                "{blocker:?}"
            );
        }
    }

    #[test]
    fn a_goal_round_yields_to_anything_the_user_did_more_recently() {
        let state = idle();
        assert_eq!(goal_step(&state, true, None, false, 25), GoalStep::Idle);
        assert_eq!(
            goal_step(&state, true, Some(3), false, 25),
            GoalStep::Continue(4)
        );
        assert_eq!(
            goal_step(&state, true, Some(3), true, 25),
            GoalStep::Achieved
        );
        assert_eq!(
            goal_step(&state, true, Some(25), false, 25),
            GoalStep::CapReached
        );
        // A queued message, a draft, an overlay or an aborted turn all
        // stop the loop continuing on its own.
        let queued = LoopState { queued: 1, ..state };
        assert_eq!(goal_step(&queued, true, Some(3), false, 25), GoalStep::Idle);
        assert_eq!(goal_step(&state, false, Some(3), false, 25), GoalStep::Idle);
        let searching = LoopState {
            modal: Some(Modal::Search),
            ..state
        };
        assert_eq!(
            goal_step(&searching, true, Some(3), false, 25),
            GoalStep::Idle
        );
    }

    /// Every gate of the warm-cache compaction, in one place: it fires
    /// exactly when the window is closing on an idle, big, unattended
    /// session, and never otherwise.
    #[test]
    fn warm_cache_compaction_fires_only_when_the_window_is_closing() {
        use std::time::{Duration, Instant};
        let now = Instant::now();
        let ready = || CacheCompactCheck {
            enabled: true,
            fired: false,
            idle_since: Some(now - Duration::from_secs(1740)),
            ttl: Duration::from_secs(1800),
            margin: Duration::from_secs(60),
            context_used: 400_000,
            context_floor: 150_000,
            deliveries_in_flight: 0,
        };
        assert!(cache_compact_due(&idle(), &ready(), now));

        // Too early: the window is not closing yet.
        let mut early = ready();
        early.idle_since = Some(now - Duration::from_secs(600));
        assert!(!cache_compact_due(&idle(), &early, now));
        // Late is still fine: a cold read compaction would pay anyway.
        let mut late = ready();
        late.idle_since = Some(now - Duration::from_secs(7200));
        assert!(cache_compact_due(&idle(), &late, now));

        let mut off = ready();
        off.enabled = false;
        assert!(!cache_compact_due(&idle(), &off, now));
        let mut spent = ready();
        spent.fired = true;
        assert!(!cache_compact_due(&idle(), &spent, now));
        let mut small = ready();
        small.context_used = 20_000;
        assert!(!cache_compact_due(&idle(), &small, now));
        let mut fresh = ready();
        fresh.idle_since = None;
        assert!(!cache_compact_due(&idle(), &fresh, now));
        let mut delivering = ready();
        delivering.deliveries_in_flight = 1;
        assert!(!cache_compact_due(&idle(), &delivering, now));

        let running = LoopState {
            turn_running: true,
            ..idle()
        };
        assert!(!cache_compact_due(&running, &ready(), now));
        let queued = LoopState {
            queued: 1,
            ..idle()
        };
        assert!(!cache_compact_due(&queued, &ready(), now));
        let modal = LoopState {
            modal: Some(Modal::Help),
            ..idle()
        };
        assert!(!cache_compact_due(&modal, &ready(), now));
    }

    #[test]
    fn a_notification_waits_for_an_idle_keyboard() {
        assert!(may_start_notification_turn(&idle()));
        assert!(!may_start_notification_turn(&LoopState {
            turn_running: true,
            ..idle()
        }));
        assert!(!may_start_notification_turn(&LoopState {
            notifications_paused: true,
            ..idle()
        }));
        // The gate that search used to slip through: a turn starting
        // under the search bar rewrites the transcript being read.
        assert!(!may_start_notification_turn(&LoopState {
            modal: Some(Modal::Search),
            ..idle()
        }));
    }

    /// The interaction the old two-block arrangement had to remember by
    /// hand: a goal round fills the prompt, so the queue must not drain
    /// over it in the same breath.
    #[test]
    fn a_goal_round_claims_the_turn_and_the_queue_waits() {
        let state = LoopState {
            queued: 0,
            ..idle()
        };
        let intents = after_turn(&state, true, Some(("ship it", 3)), false, 25);
        assert_eq!(
            intents,
            vec![
                Intent::AdvanceGoal(4),
                Intent::StartTurn(crate::goal_continuation_prompt("ship it", 4)),
            ]
        );
        // The two cannot both claim the turn: a round only continues on
        // an empty queue, which is also when the queue has nothing to
        // send. Pin that so neither guard can drift alone.
        let with_queue = LoopState { queued: 1, ..state };
        assert_eq!(
            after_turn(&with_queue, true, Some(("ship it", 3)), false, 25),
            vec![Intent::SendQueued]
        );
    }

    /// A queued message outranks a goal round: the user spoke more
    /// recently than the goal did.
    #[test]
    fn a_queued_message_wins_over_a_goal_round() {
        let state = LoopState {
            queued: 1,
            ..idle()
        };
        let intents = after_turn(&state, true, Some(("ship it", 3)), false, 25);
        assert_eq!(intents, vec![Intent::SendQueued]);
    }

    #[test]
    fn an_achieved_goal_clears_and_announces_once() {
        let intents = after_turn(&idle(), true, Some(("ship it", 2)), true, 25);
        assert_eq!(intents[0], Intent::ClearGoal);
        assert!(matches!(&intents[1], Intent::SystemLine(text) if text.contains("after 2 round")));
        // The transcript line is the announcement; no notice doubles it.
        assert_eq!(intents.len(), 2, "{intents:?}");
        assert!(!intents.iter().any(|i| matches!(i, Intent::StartTurn(_))));
    }

    #[test]
    fn a_cap_stops_the_goal_rather_than_running_another_round() {
        let intents = after_turn(&idle(), true, Some(("ship it", 25)), false, 25);
        assert_eq!(intents[0], Intent::ClearGoal);
        // In the user's words: neither the cap's internal name nor the
        // sentinel the model is asked to say.
        assert!(matches!(&intents[1], Intent::SystemLine(text)
            if text.contains("25 rounds") && text.contains("without reaching the goal")));
        assert!(
            !intents
                .iter()
                .any(|i| matches!(i, Intent::SystemLine(t) if t.contains(crate::GOAL_SENTINEL))),
            "the sentinel leaked into what the user reads"
        );
        assert!(!intents.iter().any(|i| matches!(i, Intent::StartTurn(_))));
    }

    /// An aborted or errored turn holds everything: it is not a moment
    /// to send anything on the user's behalf.
    #[test]
    fn an_unfinished_turn_starts_nothing() {
        let state = LoopState {
            queued: 2,
            ..idle()
        };
        let intents = after_turn(&state, false, Some(("ship it", 1)), false, 25);
        assert!(!intents.iter().any(|i| matches!(i, Intent::StartTurn(_))));
        assert!(!intents.contains(&Intent::SendQueued));
        assert!(matches!(
            intents.last(),
            Some(Intent::Notice(text, NoticeLevel::Warning)) if text.contains("2 queued")
        ));
    }

    /// The decision and the payload travel together: submitted text
    /// becomes exactly one intent, carrying the text.
    #[test]
    fn submitted_text_becomes_one_intent_carrying_it() {
        assert_eq!(
            submit(&idle(), false, "hi".into()),
            vec![Intent::StartTurn("hi".into())]
        );
        let steerable = LoopState {
            turn_running: true,
            steerable: true,
            ..idle()
        };
        assert_eq!(
            submit(&steerable, true, "go left".into()),
            vec![Intent::Steer("go left".into())]
        );
        let routed = LoopState {
            turn_running: true,
            steerable: false,
            ..idle()
        };
        assert_eq!(
            submit(&routed, true, "later".into()),
            vec![Intent::Queue("later".into())]
        );
    }

    /// The inverse of the rule this replaced: a submit with images
    /// attached used to come back as `PasteInput` plus a warning, which
    /// left the message in the box and the images pending. A message is
    /// a message whatever is attached to it — the images travel with it
    /// (see `apply_intent`), so the decision never mentions them.
    #[test]
    fn attachments_never_hold_a_submit_back() {
        let steerable = LoopState {
            turn_running: true,
            steerable: true,
            ..idle()
        };
        assert_eq!(
            submit(&steerable, true, "look at this".into()),
            vec![Intent::Steer("look at this".into())]
        );
        // Running but unsteerable: the queue takes it, images and all.
        let routed = LoopState {
            turn_running: true,
            steerable: false,
            ..idle()
        };
        assert_eq!(
            submit(&routed, true, "look at this".into()),
            vec![Intent::Queue("look at this".into())]
        );
        assert_eq!(
            submit(&idle(), false, "look".into()),
            vec![Intent::StartTurn("look".into())]
        );
    }

    #[test]
    fn pasted_text_becomes_the_owning_surfaces_intent_or_nothing() {
        assert_eq!(
            paste(&idle(), "text".into()),
            vec![Intent::PasteInput("text".into())]
        );
        let searching = LoopState {
            modal: Some(Modal::Search),
            ..idle()
        };
        assert_eq!(
            paste(&searching, "needle".into()),
            vec![Intent::PasteSearch("needle".into())]
        );
        let palette = LoopState {
            modal: Some(Modal::CommandPalette),
            ..idle()
        };
        assert_eq!(
            paste(&palette, "query".into()),
            vec![Intent::PastePalette("query".into())]
        );
        let picker = LoopState {
            modal: Some(Modal::ModelPicker),
            ..idle()
        };
        assert_eq!(
            paste(&picker, "text".into()),
            vec![Intent::PasteModalQuery("text".into())]
        );
        let help = LoopState {
            modal: Some(Modal::Help),
            ..idle()
        };
        assert_eq!(paste(&help, "text".into()), Vec::new());
    }

    /// Retry must not overwrite a draft — an unsubmitted one is not in
    /// the history, so it would be unrecoverable.
    #[test]
    fn retry_declines_on_a_draft_and_continues_otherwise() {
        let drafting = LoopState {
            input_blank: false,
            ..resumable()
        };
        assert!(matches!(
            retry(&drafting, false).as_slice(),
            [Intent::Notice(text, NoticeLevel::Warning)] if text.contains("draft")
        ));
        assert_eq!(retry(&resumable(), false), vec![Intent::ResumeTurn]);
    }

    /// Ctrl-R always answers: nothing armed, or a turn already
    /// running, used to be a silent keypress.
    #[test]
    fn retry_says_so_when_there_is_nothing_to_resume() {
        assert!(matches!(
            retry(&idle(), false).as_slice(),
            [Intent::Notice(text, _)] if text.contains("nothing to resume")
        ));
        let running = LoopState {
            turn_running: true,
            ..resumable()
        };
        assert!(matches!(
            retry(&running, true).as_slice(),
            [Intent::Notice(text, _)] if text.contains("already running")
        ));
        // Busy without a handle — a turn being aborted, a compaction,
        // a restore — is still busy.
        assert!(matches!(
            retry(&resumable(), true).as_slice(),
            [Intent::Notice(text, _)] if text.contains("already running")
        ));
    }

    /// The watchdog with no clock — no turn, or one aborting/paused —
    /// or with fresh data, decides nothing.
    #[test]
    fn the_stall_verdict_stays_quiet_without_a_clock_or_with_fresh_data() {
        use std::time::Duration;
        let (warn, abort) = (Duration::from_secs(300), Duration::from_secs(600));
        assert_eq!(stall_verdict(None, false, warn, abort), StallVerdict::Quiet);
        assert_eq!(
            stall_verdict(Some(Duration::from_secs(299)), false, warn, abort),
            StallVerdict::Quiet
        );
        assert_eq!(
            stall_verdict(Some(Duration::ZERO), false, warn, abort),
            StallVerdict::Quiet
        );
    }

    /// A tool call in flight holds the clock entirely: a silent tool is
    /// not a stalled provider, however long it runs. The child
    /// watchdog's false positive, not copied.
    #[test]
    fn a_tool_in_flight_holds_the_stall_clock() {
        use std::time::Duration;
        let (warn, abort) = (Duration::from_secs(300), Duration::from_secs(600));
        for silent in [300, 600, 6000] {
            assert_eq!(
                stall_verdict(Some(Duration::from_secs(silent)), true, warn, abort),
                StallVerdict::Quiet,
                "{silent}s with a tool running"
            );
        }
    }

    /// The two thresholds, edge-exact: warn at `warn_after`, abort at
    /// `abort_after`, warn in between.
    #[test]
    fn stall_silence_warns_then_aborts_at_the_thresholds() {
        use std::time::Duration;
        let (warn, abort) = (Duration::from_secs(300), Duration::from_secs(600));
        assert_eq!(
            stall_verdict(Some(Duration::from_secs(300)), false, warn, abort),
            StallVerdict::Warn { silent_secs: 300 }
        );
        assert_eq!(
            stall_verdict(Some(Duration::from_secs(599)), false, warn, abort),
            StallVerdict::Warn { silent_secs: 599 }
        );
        assert_eq!(
            stall_verdict(Some(Duration::from_secs(600)), false, warn, abort),
            StallVerdict::Abort { silent_secs: 600 }
        );
        assert_eq!(
            stall_verdict(Some(Duration::from_secs(6000)), false, warn, abort),
            StallVerdict::Abort { silent_secs: 6000 }
        );
    }

    /// The wired constants: generous (minutes, not seconds — the issue's
    /// word), and the abort strictly behind the warning so the user is
    /// always told before anything is done for them.
    #[test]
    fn the_stall_thresholds_are_generous_and_ordered() {
        use std::time::Duration;
        assert!(crate::ROOT_STALL_WARN_AFTER >= Duration::from_secs(120));
        assert!(crate::ROOT_STALL_ABORT_AFTER >= crate::ROOT_STALL_WARN_AFTER * 2);
    }

    /// The pending manager must not linger over a resumed turn: it owns
    /// the keyboard and blocks notification routing until dismissed.
    /// The draft warning is the one case that keeps it open, so the
    /// user can act on it where they raised it.
    #[test]
    fn retry_dismisses_the_manager_only_when_it_resumes() {
        assert!(retry_dismisses_manager(&retry(&resumable(), false)));
        let drafting = LoopState {
            input_blank: false,
            ..resumable()
        };
        assert!(!retry_dismisses_manager(&retry(&drafting, false)));
    }

    fn edges(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(id, parent)| (id.to_string(), parent.to_string()))
            .collect()
    }

    /// A chain nests one level per listed ancestor; a fork keeps
    /// siblings level with each other.
    #[test]
    fn tree_depths_nest_chains_and_keep_siblings_level() {
        // root's children (parent unlisted) → 0; the chain climbs.
        let chain = edges(&[("a", "root"), ("b", "a"), ("c", "b")]);
        assert_eq!(tree_depths(&chain), vec![0, 1, 2]);

        let fork = edges(&[("a", "root"), ("b", "a"), ("c", "a"), ("d", "root")]);
        assert_eq!(tree_depths(&fork), vec![0, 1, 1, 0]);
    }

    /// A parent nobody listed — a foreign tree's root — anchors at 0,
    /// and its own descendants still nest under it.
    #[test]
    fn tree_depths_anchor_foreign_roots_at_zero() {
        let foreign = edges(&[("x", "elsewhere"), ("y", "x")]);
        assert_eq!(tree_depths(&foreign), vec![0, 1]);
    }

    /// Ancestry that loops — self-references included — must terminate,
    /// not hang the render loop that asked.
    #[test]
    fn tree_depths_refuse_to_walk_a_cycle_forever() {
        assert_eq!(tree_depths(&edges(&[("a", "a")])), vec![0]);
        // a→b→a: each stops when the walk comes back around.
        let cycle = edges(&[("a", "b"), ("b", "a"), ("c", "a")]);
        assert_eq!(tree_depths(&cycle), vec![1, 1, 2]);
    }

    /// A session listed twice (its delivery row beside its turn row)
    /// keys by the first occurrence: both rows get one depth, and a
    /// child of that session nests under it once.
    #[test]
    fn tree_depths_key_a_duplicated_session_by_first_occurrence() {
        let doubled = edges(&[("a", "root"), ("a", "ghost"), ("b", "a")]);
        assert_eq!(tree_depths(&doubled), vec![0, 0, 1]);
    }

    fn press(code: crossterm::event::KeyCode) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }

    /// The whole offer in one test: Enter on an empty prompt takes it,
    /// typing leaves it behind at the first character, Esc on an empty
    /// prompt dismisses it, and nothing else touches it.
    #[test]
    fn an_offered_session_answers_enter_and_esc_and_nothing_else() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let drafting = LoopState {
            input_blank: false,
            ..idle()
        };

        assert_eq!(
            ghost_step(&idle(), press(KeyCode::Enter)),
            GhostStep::Resume
        );
        assert_eq!(
            ghost_step(&drafting, press(KeyCode::Enter)),
            GhostStep::Dismiss,
            "a message sent goes to the fresh session, and the offer is answered"
        );
        assert_eq!(ghost_step(&idle(), press(KeyCode::Esc)), GhostStep::Dismiss);
        assert_eq!(
            ghost_step(&drafting, press(KeyCode::Esc)),
            GhostStep::Keep,
            "Esc over a draft clears the draft; one press, one thing"
        );
        assert_eq!(
            ghost_step(
                &idle(),
                crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)
            ),
            GhostStep::Keep,
            "Shift-Enter is a newline, not a send"
        );
        for code in [KeyCode::Char('x'), KeyCode::Char(' ')] {
            assert_eq!(
                ghost_step(&idle(), press(code)),
                GhostStep::Dismiss,
                "{code:?} starts a fresh message, so the offer goes"
            );
        }
        for code in [KeyCode::Up, KeyCode::PageUp, KeyCode::F(1)] {
            assert_eq!(
                ghost_step(&idle(), press(code)),
                GhostStep::Keep,
                "{code:?} neither types nor answers"
            );
        }
        for shortcut in [
            crossterm::event::KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            crossterm::event::KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT),
            // A kitty-protocol terminal reports an unbound Cmd-key
            // this way; nothing is typed, so nothing is answered.
            crossterm::event::KeyEvent::new(KeyCode::Char('c'), KeyModifiers::SUPER),
        ] {
            assert_eq!(
                ghost_step(&idle(), shortcut),
                GhostStep::Keep,
                "{shortcut:?} is a shortcut, not typing"
            );
        }
    }

    /// A modal in front owns the keyboard: its own Enter and Esc are
    /// not the offer's, or opening the picker over an offer would
    /// resume the wrong session on the first Enter. The Ctrl-X leader
    /// owns them too — it draws nothing, so nothing else would notice.
    #[test]
    fn a_modal_in_front_keeps_the_offer_out_of_the_keyboard() {
        use crossterm::event::KeyCode;
        let leader = LoopState {
            model_key_pending: true,
            ..idle()
        };
        for covered in [Modal::SessionPicker, Modal::Help, Modal::CommandPalette]
            .into_iter()
            .map(|modal| LoopState {
                modal: Some(modal),
                ..idle()
            })
            .chain(std::iter::once(leader))
        {
            assert_eq!(ghost_step(&covered, press(KeyCode::Enter)), GhostStep::Keep);
            assert_eq!(ghost_step(&covered, press(KeyCode::Esc)), GhostStep::Keep);
        }
    }
}
