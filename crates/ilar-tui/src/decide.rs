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
    PasteInput(String),
    /// Drop the goal, having finished or run out of rounds.
    ClearGoal,
    /// Advance the goal to this round.
    AdvanceGoal(u32),
    /// A line in the transcript.
    SystemLine(String),
    Notice(String, NoticeLevel),
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
        // Spelled out rather than a wildcard: a new modal with a filter
        // must fail to compile here instead of silently swallowing
        // pastes, the way the pickers used to.
        Some(
            Modal::Help
            | Modal::Todos
            | Modal::Aside
            | Modal::PendingManager
            | Modal::SkillPicker
            | Modal::VariantPicker,
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
            intents.push(Intent::SystemLine(message.clone()));
            intents.push(Intent::Notice(message, NoticeLevel::Info));
        }
        GoalStep::CapReached => {
            let message = format!(
                "goal round cap ({max_rounds}) reached without {} — stopping",
                crate::GOAL_SENTINEL
            );
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
            return vec![Intent::Notice(
                format!("wait for the current operation before /{name}"),
                NoticeLevel::Warning,
            )];
        }
    }
    match submit_target(state, busy) {
        SubmitTarget::StartTurn => vec![Intent::StartTurn(text)],
        SubmitTarget::Steer => vec![Intent::Steer(text)],
        SubmitTarget::Queue => vec![Intent::Queue(text)],
    }
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
        PasteTarget::Input => vec![Intent::PasteInput(text)],
        PasteTarget::Discard => Vec::new(),
    }
}

/// Resume the failed turn, or preserve an unsent draft.
pub(crate) fn retry(state: &LoopState) -> Vec<Intent> {
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
pub(crate) fn may_start_notification_turn(state: &LoopState) -> bool {
    !state.turn_running && !state.notifications_paused && state.modal.is_none()
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
        first_occurrence
            .entry(session_id.as_str())
            .or_insert(index);
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
            Modal::Help,
            Modal::Todos,
            Modal::Aside,
            Modal::PendingManager,
            Modal::SkillPicker,
            Modal::VariantPicker,
        ] {
            let state = LoopState {
                modal: Some(modal),
                ..idle()
            };
            assert_eq!(paste_target(&state), PasteTarget::Discard, "{modal:?}");
            assert_eq!(paste(&state, "needle".into()), Vec::new(), "{modal:?}");
        }
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
        assert!(matches!(&intents[2], Intent::Notice(_, NoticeLevel::Info)));
        assert!(!intents.iter().any(|i| matches!(i, Intent::StartTurn(_))));
    }

    #[test]
    fn a_cap_stops_the_goal_rather_than_running_another_round() {
        let intents = after_turn(&idle(), true, Some(("ship it", 25)), false, 25);
        assert_eq!(intents[0], Intent::ClearGoal);
        assert!(matches!(&intents[1], Intent::SystemLine(text) if text.contains("cap (25)")));
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
            ..idle()
        };
        assert!(matches!(
            retry(&drafting).as_slice(),
            [Intent::Notice(text, NoticeLevel::Warning)] if text.contains("draft")
        ));
        assert_eq!(retry(&idle()), vec![Intent::ResumeTurn]);
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
        assert!(retry_dismisses_manager(&retry(&idle())));
        let drafting = LoopState {
            input_blank: false,
            ..idle()
        };
        assert!(!retry_dismisses_manager(&retry(&drafting)));
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
}
