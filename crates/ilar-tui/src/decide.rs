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

/// Where pasted text goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PasteTarget {
    Palette,
    Search,
    Input,
    /// A modal with nowhere to put it.
    Discard,
}

pub(crate) fn paste_target(state: &LoopState) -> PasteTarget {
    match state.modal {
        Some(Modal::CommandPalette) => PasteTarget::Palette,
        Some(Modal::Search) => PasteTarget::Search,
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
}
