//! The loop's schedule as a function.
//!
//! `decide` covers what should happen; nothing covered *when*. The
//! queue-inversion bug lived in the ordering — the notification gate
//! firing before the queue drain in the same iteration — and no
//! decision-level test could see it, because each decision was
//! individually correct. `settle` is that stretch of the iteration as
//! one function: drain the decided intents, let a buffered palette
//! shortcut open, then gate notifications on the *result*. Tests drive
//! it with a fake [`Runtime`] and assert on the sequence.
//!
//! Everything effectful stays behind the trait: `run_app` implements
//! it over tokio spawns and crossterm, tests implement it with a
//! recorder whose `turn_running` flips when a turn starts — which is
//! exactly what makes a reordering visible.

use crate::app::App;
use crate::decide::{Intent, LoopState, after_turn, may_route_notification};
use crate::transcript::Line_;
use crate::{Activity, MAX_GOAL_ROUNDS, NoticeLevel};
use ilar::agent::TurnOutcome;
use ilar::subagent::{Notification, RouteOutcome};

/// How the turn that was running ended — the edge hands this in after
/// awaiting the join, so the pass itself never blocks.
pub(crate) enum Completion {
    Root(anyhow::Result<TurnOutcome>),
    Routed(anyhow::Result<RouteOutcome>),
    /// The turn task itself died.
    Crashed(String),
}

/// The effectful edges of one settle pass.
pub(crate) trait Runtime {
    /// The loop state as the gate must see it, mid-pass: `turn_running`
    /// reflects a turn the drain started moments ago.
    fn observe(&self, app: &App) -> LoopState;
    /// Apply one intent, spawning a turn if it yields a prompt.
    fn perform(&mut self, app: &mut App, intent: Intent) -> anyhow::Result<()>;
    /// Between the drain and the gate: give a buffered Ctrl-P the
    /// chance to open the palette before a notification could claim
    /// the keyboard.
    fn peek_palette(&mut self, app: &mut App) -> anyhow::Result<()>;
    /// The next notification to act on; `held` means the gate is
    /// closed and nothing may be consumed.
    fn next_notification(&mut self, held: bool) -> Option<Notification>;
    /// A notification for another session: hand it off.
    fn route(&mut self, app: &mut App, notification: Notification);
    /// A notification for this session: start its turn here.
    fn start_notification_turn(&mut self, app: &mut App, notification: Notification);
    fn session_id(&self) -> &str;
    /// A turn ended without an abort: notifications flow again.
    fn resume_notifications(&mut self);
    /// A routed notification asked to wait for the user.
    fn pause_notifications(&mut self);
    /// A routed notification came back for this session: hold it
    /// behind whatever was already queued ahead of it.
    fn hold_propagate(&mut self, notification: Notification);
    /// A requeued notification goes to the front, to be re-offered as
    /// soon as the user resumes.
    fn hold_requeue(&mut self, notification: Notification);
    /// Drop the ended turn's channels; a steer sent after this queues.
    fn end_turn(&mut self);
    /// Persist and adopt the pre-override model — the tail end of a
    /// command's one-turn override.
    fn revert_model(&mut self, app: &mut App, model: String, variant: Option<String>);
}

/// What the caller does after a settle pass.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Settled {
    Proceed,
    /// A routing turn was spawned; restart the iteration so its
    /// completion is awaited before anything else happens.
    Restart,
}

/// A whole pass: fold the completion that triggered it (if any) into
/// the intents, then settle. This is the iteration's spine — what a
/// turn ending sets in motion, in the order it must happen:
/// completion bookkeeping and `after_turn` decisions first, then the
/// drain, then the gate on the drain's result.
pub(crate) fn pass<R: Runtime>(
    app: &mut App,
    completion: Option<Completion>,
    carried: Vec<Intent>,
    runtime: &mut R,
) -> anyhow::Result<Settled> {
    let mut intents = carried;
    if let Some(completion) = completion {
        intents.extend(complete(app, completion, runtime));
    }
    settle(app, intents, runtime)
}

/// A turn ended: bookkeeping, then what `after_turn` decides. The
/// internal order is load-bearing and deliberate:
/// - `after_turn` observes *before* undelivered steers are spliced
///   into the queue, so a steer the turn never saw waits for the user
///   instead of auto-sending after an abort.
/// - the model revert runs *before* the drain the caller will do
///   next, so a queued turn starts under the reverted model, not the
///   override's.
fn complete<R: Runtime>(app: &mut App, completion: Completion, runtime: &mut R) -> Vec<Intent> {
    let mut intents = Vec::new();
    match completion {
        Completion::Root(result) => {
            let aborted = matches!(result, Ok(TurnOutcome::Aborted));
            let completed = matches!(result, Ok(TurnOutcome::Completed));
            app.finish_turn(result);
            if !aborted {
                runtime.resume_notifications();
            }
            if aborted && let Some((_, round)) = &app.goal {
                let message = format!(
                    "goal paused (round {round}/{MAX_GOAL_ROUNDS}) — resumes after your next completed turn; Ctrl-Q to manage"
                );
                app.push_transcript_line(Line_::System(message.clone()));
                app.set_notice(message, NoticeLevel::Warning);
            }
            let state = runtime.observe(app);
            let round = app.goal.as_ref().map(|(_, round)| *round);
            // Only scan the transcript when there is a goal to
            // satisfy; every other turn pays nothing.
            let achieved = round.is_some()
                && app
                    .lines
                    .iter()
                    .rev()
                    .find_map(|line| match line {
                        Line_::Assistant(text) => Some(crate::goal_achieved_in(text)),
                        _ => None,
                    })
                    .unwrap_or(false);
            let goal = app
                .goal
                .as_ref()
                .map(|(goal, round)| (goal.clone(), *round));
            intents = after_turn(
                &state,
                completed,
                goal.as_ref().map(|(goal, round)| (goal.as_str(), *round)),
                achieved,
                MAX_GOAL_ROUNDS,
            );
        }
        Completion::Routed(Ok(RouteOutcome::Propagate(notification))) => {
            app.busy = false;
            app.status = "ready".into();
            app.clear_transient_notice();
            app.set_activity(Activity::Ready);
            runtime.hold_propagate(notification);
        }
        Completion::Routed(Ok(RouteOutcome::Requeue(notification))) => {
            app.busy = false;
            app.status = "notification paused; send a message to resume".into();
            app.set_persistent_notice(
                "notification paused; send a message to resume",
                NoticeLevel::Warning,
            );
            app.set_activity(Activity::Paused);
            runtime.hold_requeue(notification);
            runtime.pause_notifications();
        }
        Completion::Routed(Ok(RouteOutcome::Complete)) => {
            app.busy = false;
            app.status = "ready".into();
            app.clear_transient_notice();
            app.set_activity(Activity::Ready);
        }
        Completion::Routed(Err(error)) => {
            app.busy = false;
            app.status = "error".into();
            app.set_activity(Activity::Error);
            let message = format!("notification routing failed: {error}");
            app.set_notice(&message, NoticeLevel::Error);
            app.push_transcript_line(Line_::System(message));
        }
        Completion::Crashed(error) => {
            app.busy = false;
            app.status = "error".into();
            app.set_activity(Activity::Error);
            let message = format!("notification routing failed: {error}");
            app.set_notice(&message, NoticeLevel::Error);
            app.push_transcript_line(Line_::System(message));
        }
    }
    runtime.end_turn();
    // The turn dropped its receiver. Anything it never delivered
    // (an abort, an error) would otherwise vanish with no
    // transcript line and no way to get it back.
    if !app.pending_steers.is_empty() {
        let undelivered = std::mem::take(&mut app.pending_steers);
        let count = undelivered.len();
        app.queued_messages.splice(0..0, undelivered);
        app.set_notice(
            format!("{count} undelivered steer(s) moved to the queue — Ctrl-Q to review"),
            NoticeLevel::Warning,
        );
    }
    // A command's model override ends with its turn — however the
    // turn ended.
    if let Some((model, variant)) = app.model_revert.take() {
        runtime.revert_model(app, model, variant);
    }
    intents
}

/// One settle pass, in the order that defines the schedule. The order
/// is the point: the gate must observe a turn the drain just started,
/// or a background notification steals the turn a queued message was
/// promised — the recorded queue-inversion bug.
pub(crate) fn settle<R: Runtime>(
    app: &mut App,
    intents: Vec<Intent>,
    runtime: &mut R,
) -> anyhow::Result<Settled> {
    for intent in intents {
        runtime.perform(app, intent)?;
    }
    runtime.peek_palette(app)?;
    let state = runtime.observe(app);
    if let Some(notification) = runtime.next_notification(!may_route_notification(&state)) {
        if notification.parent_session_id != runtime.session_id() {
            runtime.route(app, notification);
            return Ok(Settled::Restart);
        }
        runtime.start_notification_turn(app, notification);
    }
    Ok(Settled::Proceed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// Mirrors the real runtime's shape: `perform` goes through the
    /// real `apply_intent`, and starting any turn flips `turn_running`
    /// — the flag the gate reads. That flip is what a reordered
    /// schedule gets wrong.
    struct FakeRuntime {
        session_id: String,
        turn_running: bool,
        paused: bool,
        pending: VecDeque<Notification>,
        log: Vec<String>,
    }

    impl FakeRuntime {
        fn new() -> Self {
            Self {
                session_id: "root".into(),
                turn_running: false,
                paused: false,
                pending: VecDeque::new(),
                log: Vec::new(),
            }
        }

        fn with_notification(mut self, parent: &str, text: &str) -> Self {
            self.pending.push_back(Notification {
                parent_session_id: parent.into(),
                description: "background task".into(),
                text: text.into(),
                is_error: false,
            });
            self
        }
    }

    impl Runtime for FakeRuntime {
        fn observe(&self, app: &App) -> LoopState {
            LoopState {
                turn_running: self.turn_running,
                modal: app.active_modal(),
                input_blank: app.input.is_blank(),
                pending_event: false,
                queued: app.queued_messages.len(),
                steerable: false,
                notifications_paused: self.paused,
            }
        }

        fn perform(&mut self, app: &mut App, intent: Intent) -> anyhow::Result<()> {
            if let Some(text) = crate::apply_intent(app, intent, None) {
                self.turn_running = true;
                self.log.push(format!("start_turn:{text}"));
            }
            Ok(())
        }

        fn peek_palette(&mut self, _app: &mut App) -> anyhow::Result<()> {
            Ok(())
        }

        fn next_notification(&mut self, held: bool) -> Option<Notification> {
            if held {
                return None;
            }
            self.pending.pop_front()
        }

        fn route(&mut self, _app: &mut App, notification: Notification) {
            self.turn_running = true;
            self.log
                .push(format!("route:{}", notification.parent_session_id));
        }

        fn start_notification_turn(&mut self, _app: &mut App, notification: Notification) {
            self.turn_running = true;
            self.log.push(format!("notify_turn:{}", notification.text));
        }

        fn session_id(&self) -> &str {
            &self.session_id
        }

        fn resume_notifications(&mut self) {
            self.paused = false;
        }

        fn pause_notifications(&mut self) {
            self.paused = true;
        }

        fn hold_propagate(&mut self, notification: Notification) {
            self.log.push(format!("hold:{}", notification.text));
            self.pending.push_back(notification);
        }

        fn hold_requeue(&mut self, notification: Notification) {
            self.log.push(format!("requeue:{}", notification.text));
            self.pending.push_front(notification);
        }

        fn end_turn(&mut self) {
            self.log.push("end_turn".into());
        }

        fn revert_model(&mut self, app: &mut App, model: String, variant: Option<String>) {
            self.log.push(format!("revert:{model}"));
            app.current_model = model;
            app.current_variant = variant;
        }
    }

    /// The recorded queue-inversion bug, pinned: a turn completes with
    /// a message queued AND a notification waiting. The queued message
    /// must get the turn; the notification must stay for later, not be
    /// consumed and not start a turn of its own. Reordering the gate
    /// before the drain in `settle` fails this.
    #[test]
    fn a_dequeued_message_outranks_a_notification_in_the_same_pass() {
        let mut app = App::new();
        app.queued_messages = vec!["do the next thing".into()];
        let mut runtime = FakeRuntime::new().with_notification("root", "task finished");

        let outcome = settle(&mut app, vec![Intent::SendQueued], &mut runtime).unwrap();

        assert_eq!(outcome, Settled::Proceed);
        assert_eq!(runtime.log, vec!["start_turn:do the next thing"]);
        assert_eq!(
            runtime.pending.len(),
            1,
            "the notification must wait, not vanish"
        );
        assert!(app.queued_messages.is_empty());
    }

    /// A queued slash invocation expands through the schedule, not just
    /// through `apply_intent` in isolation: what the runtime is told to
    /// start is the expansion, never the literal text.
    #[test]
    fn a_queued_slash_invocation_reaches_the_runtime_expanded() {
        let mut app = App::new();
        app.queued_messages = vec!["/goal ship the parser".into()];
        let mut runtime = FakeRuntime::new();

        settle(&mut app, vec![Intent::SendQueued], &mut runtime).unwrap();

        assert_eq!(runtime.log.len(), 1);
        let started = &runtime.log[0];
        assert!(started.starts_with("start_turn:"), "{started}");
        assert!(
            !started.contains("/goal ship the parser"),
            "the literal command leaked to the model: {started}"
        );
        assert!(started.contains("ship the parser"), "{started}");
    }

    /// With an overlay holding the keyboard, the pass loses nothing: a
    /// held queue stays queued and the notification stays pending. The
    /// old synthetic-Enter versions of this dropped the message into
    /// whatever owned the keyboard.
    #[test]
    fn a_modal_holds_both_the_queue_and_the_gate_without_losing_either() {
        let mut app = App::new();
        app.queued_messages = vec!["held message".into()];
        app.help_visible = true;
        let mut runtime = FakeRuntime::new().with_notification("root", "task finished");

        // What after_turn decides under a modal: hold, with a notice.
        let state = runtime.observe(&app);
        let intents = crate::decide::after_turn(&state, true, None, false, 25);
        assert!(!intents.contains(&Intent::SendQueued));
        settle(&mut app, intents, &mut runtime).unwrap();

        assert!(runtime.log.is_empty(), "{:?}", runtime.log);
        assert_eq!(app.queued_messages, vec!["held message"]);
        assert_eq!(runtime.pending.len(), 1);
    }

    /// The whole spine at once: a completing turn with a message
    /// queued and a notification waiting must decide, drain and gate
    /// in that order — the queued message gets the turn, the
    /// notification waits. Folding the completion in after the settle
    /// instead of before it fails this.
    #[test]
    fn a_completion_decides_before_the_drain_and_the_gate() {
        let mut app = App::new();
        app.busy = true;
        app.queued_messages = vec!["do the next thing".into()];
        let mut runtime = FakeRuntime::new().with_notification("root", "task finished");

        let outcome = pass(
            &mut app,
            Some(Completion::Root(Ok(TurnOutcome::Completed))),
            Vec::new(),
            &mut runtime,
        )
        .unwrap();

        assert_eq!(outcome, Settled::Proceed);
        assert_eq!(
            runtime.log,
            vec!["end_turn", "start_turn:do the next thing"]
        );
        assert_eq!(runtime.pending.len(), 1, "the notification waits");
        assert!(app.queued_messages.is_empty());
    }

    /// A command's model override ends with its turn: the revert runs
    /// before the drain, so a queued follow-up starts under the
    /// reverted model — a property nothing pinned until now.
    #[test]
    fn a_queued_turn_starts_under_the_reverted_model() {
        let mut app = App::new();
        app.busy = true;
        app.current_model = "override/model".into();
        app.model_revert = Some(("original/model".into(), None));
        app.queued_messages = vec!["follow-up".into()];
        let mut runtime = FakeRuntime::new();

        pass(
            &mut app,
            Some(Completion::Root(Ok(TurnOutcome::Completed))),
            Vec::new(),
            &mut runtime,
        )
        .unwrap();

        assert_eq!(
            runtime.log,
            vec!["end_turn", "revert:original/model", "start_turn:follow-up"]
        );
        assert_eq!(app.current_model, "original/model");
        assert!(app.model_revert.is_none());
    }

    /// An abort resumes nothing on the user's behalf: notifications
    /// stay paused, the queue holds, and nothing starts. A completed
    /// turn resumes the flow — and the freed gate admits a waiting
    /// notification in the same pass.
    #[test]
    fn an_abort_holds_everything_a_completion_resumes_the_flow() {
        let mut app = App::new();
        app.busy = true;
        app.queued_messages = vec!["held".into()];
        let mut runtime = FakeRuntime::new().with_notification("root", "task finished");
        runtime.paused = true;

        pass(
            &mut app,
            Some(Completion::Root(Ok(TurnOutcome::Aborted))),
            Vec::new(),
            &mut runtime,
        )
        .unwrap();
        assert_eq!(runtime.log, vec!["end_turn"]);
        assert!(runtime.paused, "an abort must not resume notifications");
        assert_eq!(app.queued_messages, vec!["held"]);
        assert_eq!(runtime.pending.len(), 1);

        // The next completed turn resumes, and the gate opens in the
        // same pass. (Queue emptied: a queued message would outrank.)
        app.busy = true;
        app.queued_messages.clear();
        runtime.log.clear();
        pass(
            &mut app,
            Some(Completion::Root(Ok(TurnOutcome::Completed))),
            Vec::new(),
            &mut runtime,
        )
        .unwrap();
        assert!(!runtime.paused);
        assert_eq!(runtime.log, vec!["end_turn", "notify_turn:task finished"]);
    }

    /// Steers the turn never consumed return to the queue and wait for
    /// the user: `after_turn` observes before the splice, so nothing
    /// auto-sends what the user aimed at a turn that no longer exists.
    #[test]
    fn undelivered_steers_return_to_the_queue_and_wait() {
        let mut app = App::new();
        app.busy = true;
        app.pending_steers = vec!["go left".into()];
        let mut runtime = FakeRuntime::new();

        pass(
            &mut app,
            Some(Completion::Root(Ok(TurnOutcome::Completed))),
            Vec::new(),
            &mut runtime,
        )
        .unwrap();

        assert_eq!(runtime.log, vec!["end_turn"], "nothing starts");
        assert_eq!(app.queued_messages, vec!["go left"]);
        assert!(app.pending_steers.is_empty());
    }

    /// A goal continues through the whole pass: the continuation turn
    /// starts and the round advances.
    #[test]
    fn a_goal_round_continues_through_the_pass() {
        let mut app = App::new();
        app.busy = true;
        app.goal = Some(("ship the parser".into(), 2));
        let mut runtime = FakeRuntime::new();

        pass(
            &mut app,
            Some(Completion::Root(Ok(TurnOutcome::Completed))),
            Vec::new(),
            &mut runtime,
        )
        .unwrap();

        assert_eq!(runtime.log.len(), 2, "{:?}", runtime.log);
        assert_eq!(runtime.log[0], "end_turn");
        assert!(
            runtime.log[1].starts_with("start_turn:") && runtime.log[1].contains("ship the parser"),
            "{:?}",
            runtime.log
        );
        assert_eq!(app.goal.as_ref().map(|(_, round)| *round), Some(3));
    }

    /// A routed notification that asks to wait pauses the gate and is
    /// held at the front; nothing else moves.
    #[test]
    fn a_requeued_routing_pauses_the_gate() {
        let mut app = App::new();
        app.busy = true;
        let mut runtime = FakeRuntime::new();
        let notification = Notification {
            parent_session_id: "root".into(),
            description: "background task".into(),
            text: "needs the user".into(),
            is_error: false,
        };

        pass(
            &mut app,
            Some(Completion::Routed(Ok(RouteOutcome::Requeue(notification)))),
            Vec::new(),
            &mut runtime,
        )
        .unwrap();

        assert!(runtime.paused);
        assert_eq!(runtime.log, vec!["requeue:needs the user", "end_turn"]);
        assert_eq!(runtime.pending.len(), 1, "held, not delivered");
        assert!(!app.busy);
    }

    /// An idle pass lets the notification through: same-session starts
    /// its turn here, foreign routes and restarts the iteration.
    #[test]
    fn an_idle_pass_admits_the_notification() {
        let mut app = App::new();
        let mut runtime = FakeRuntime::new().with_notification("root", "task finished");
        let outcome = settle(&mut app, Vec::new(), &mut runtime).unwrap();
        assert_eq!(outcome, Settled::Proceed);
        assert_eq!(runtime.log, vec!["notify_turn:task finished"]);

        let mut app = App::new();
        let mut runtime = FakeRuntime::new().with_notification("elsewhere", "done");
        let outcome = settle(&mut app, Vec::new(), &mut runtime).unwrap();
        assert_eq!(outcome, Settled::Restart);
        assert_eq!(runtime.log, vec!["route:elsewhere"]);
    }
}
