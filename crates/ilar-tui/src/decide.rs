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
    /// Hold until the running turn completes.
    Queue(String),
    /// Pasted text, routed to whichever surface owned the keyboard.
    PastePalette(String),
    PasteSearch(String),
    PasteQuestion(String),
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
    Input,
    /// A modal with nowhere to put it.
    Discard,
}

pub(crate) fn paste_target(state: &LoopState) -> PasteTarget {
    match state.modal {
        Some(Modal::CommandPalette) => PasteTarget::Palette,
        Some(Modal::Search) => PasteTarget::Search,
        Some(Modal::Question) => PasteTarget::Question,
        Some(_) => PasteTarget::Discard,
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

/// What a submitted prompt becomes. The decision (`submit_target`) and
/// the payload travel together, so a call site cannot route the text
/// one way while believing it decided another.
pub(crate) fn submit(state: &LoopState, busy: bool, text: String) -> Vec<Intent> {
    // Maintenance commands must never become steering text for the model.
    if let Some((name @ ("compact" | "rewind" | "fork" | "sessions"), args)) =
        crate::parse_slash_invocation(&text)
    {
        if !args.is_empty() {
            return vec![Intent::Notice(
                format!("usage: /{name}"),
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

/// What pasted text becomes. A picker returns nothing: it has nowhere
/// to put text, and falling through to the prompt behind it would edit
/// something the user cannot see.
pub(crate) fn paste(state: &LoopState, text: String) -> Vec<Intent> {
    match paste_target(state) {
        PasteTarget::Palette => vec![Intent::PastePalette(text)],
        PasteTarget::Search => vec![Intent::PasteSearch(text)],
        PasteTarget::Question => vec![Intent::PasteQuestion(text)],
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

/// Whether a background completion may start a turn now. An overlay
/// owning the keyboard counts: a turn starting underneath a picker or
/// the search bar moves the transcript out from under the user.
pub(crate) fn may_route_notification(state: &LoopState) -> bool {
    !state.turn_running && !state.notifications_paused && state.modal.is_none()
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
        // A picker has nowhere to put it; it must not fall through to
        // the prompt behind it.
        let picker = LoopState {
            modal: Some(Modal::ModelPicker),
            ..idle()
        };
        assert_eq!(paste_target(&picker), PasteTarget::Discard);
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
        assert!(may_route_notification(&idle()));
        assert!(!may_route_notification(&LoopState {
            turn_running: true,
            ..idle()
        }));
        assert!(!may_route_notification(&LoopState {
            notifications_paused: true,
            ..idle()
        }));
        // The gate that search used to slip through: a turn starting
        // under the search bar rewrites the transcript being read.
        assert!(!may_route_notification(&LoopState {
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
        assert_eq!(paste(&picker, "text".into()), Vec::new());
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
}
