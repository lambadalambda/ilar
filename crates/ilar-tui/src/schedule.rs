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
use crate::decide::{
    Intent, LoopState, QueueStep, after_turn, may_start_notification_turn, queue_step,
};
use crate::transcript::Line_;
use crate::{Activity, MAX_GOAL_ROUNDS, NoticeLevel};
use ilar::agent::TurnOutcome;
use ilar::compaction::ManualCompactionOutcome;
use ilar::subagent::{Notification, RouteOutcome};

/// How the operation that was running ended — the edge hands this in after
/// awaiting the join, so the pass itself never blocks.
pub(crate) enum Completion {
    Root(anyhow::Result<TurnOutcome>),
    /// A detached delivery to another session finished. Carries the
    /// notification it was delivering, so a failure can still put the
    /// child's final word in front of the user instead of losing it
    /// with the plumbing error.
    Routed {
        result: anyhow::Result<RouteOutcome>,
        notification: Notification,
    },
    Compaction(anyhow::Result<ManualCompactionOutcome>),
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
    /// The next notification waiting, held-back ones first.
    fn next_notification(&mut self) -> Option<Notification>;
    /// Start an explicitly requested idle-session compaction.
    fn start_compaction(&mut self, app: &mut App);
    /// Ask a `/btw` question over the session, off the record.
    fn start_aside(&mut self, app: &mut App, question: String);
    /// A notification for another session: spawn its delivery beside
    /// whatever else is running. It resumes a child, so it takes
    /// neither the turn slot nor the keyboard, and several may run.
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
    /// Same-session notifications the gate could not admit this pass,
    /// in arrival order: put them back in front so nothing is lost
    /// and order holds.
    fn hold_blocked(&mut self, notifications: Vec<Notification>);
    /// Drop the ended turn's channels; a steer sent after this queues.
    fn end_turn(&mut self);
    /// Persist and adopt the pre-override model — the tail end of a
    /// command's one-turn override.
    fn revert_model(&mut self, app: &mut App, model: String, variant: Option<String>);
    /// A subtask command spawns detached; only session setup runs
    /// before this returns.
    async fn start_subtask(&mut self, app: &mut App, request: crate::app::SubtaskRequest);
    /// Everything the user sees for this pass: the bell, the counts,
    /// the frame.
    fn present(&mut self, app: &mut App) -> anyhow::Result<()>;
    /// Wait briefly for the next terminal event (fast while busy, so
    /// streaming keeps rendering).
    fn poll_event(&mut self, busy: bool) -> anyhow::Result<Option<crossterm::event::Event>>;
}

/// How a tick ended, and what the caller owes the iteration.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Tick {
    /// Nothing arrived within the poll window.
    Idle,
    /// A terminal event for the dispatch half, which stays outside
    /// this seam: its decisions are already under test in `decide` and
    /// the modal handlers, and its effects are session-store and
    /// terminal I/O that a fake could only mirror, not check.
    Dispatch(crossterm::event::Event),
}

/// One whole iteration, minus the dispatch: the pass, the subtask
/// spawn, the frame, the poll. The frame sits between the pass and
/// the poll by construction — a click is always mapped through the
/// hit map of the frame the user actually saw.
pub(crate) async fn tick<R: Runtime>(
    app: &mut App,
    completions: Vec<Completion>,
    carried: Vec<Intent>,
    runtime: &mut R,
) -> anyhow::Result<Tick> {
    pass(app, completions, carried, runtime)?;
    // After the drain — a queued command may have armed it there.
    if let Some(request) = app.pending_subtask.take() {
        runtime.start_subtask(app, request).await;
    }
    runtime.present(app)?;
    match runtime.poll_event(app.busy)? {
        Some(event) => Ok(Tick::Dispatch(event)),
        None => Ok(Tick::Idle),
    }
}

/// A whole pass: fold the completions that triggered it (the turn's
/// and any finished deliveries') into the intents, then settle. This
/// is the iteration's spine — what a turn ending sets in motion, in
/// the order it must happen: completion bookkeeping and `after_turn`
/// decisions first, then the drain, then the gate on the drain's
/// result.
pub(crate) fn pass<R: Runtime>(
    app: &mut App,
    completions: Vec<Completion>,
    carried: Vec<Intent>,
    runtime: &mut R,
) -> anyhow::Result<()> {
    let mut intents = carried;
    for completion in completions {
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
        // A delivery's completion is bookkeeping for the delivery
        // alone. It never ran in the turn slot, so none of the turn
        // teardown below — end_turn, the steer splice, the model
        // revert — is its to trigger: a root turn may be running
        // right now, and end_turn would tear that turn's channels
        // down.
        Completion::Routed {
            result,
            notification,
        } => {
            routed_complete(app, result, notification, runtime);
            return Vec::new();
        }
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
                    .lines()
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
        Completion::Compaction(result) => {
            let completed = matches!(
                &result,
                Ok(ManualCompactionOutcome::Compacted { .. })
                    | Ok(ManualCompactionOutcome::NothingToCompact)
            );
            match result {
                Ok(ManualCompactionOutcome::Compacted {
                    summary,
                    context_tokens,
                }) => {
                    app.push_loop_event(&ilar::agent::LoopEvent::Compacted {
                        context_tokens,
                        summary,
                    });
                    app.busy = false;
                    app.status = "ready".into();
                    app.set_activity(Activity::Ready);
                    app.set_notice("compaction complete", NoticeLevel::Info);
                }
                Ok(ManualCompactionOutcome::NothingToCompact) => {
                    app.busy = false;
                    app.status = "ready".into();
                    app.set_activity(Activity::Ready);
                    app.set_notice("nothing to compact", NoticeLevel::Info);
                }
                Ok(ManualCompactionOutcome::Aborted) => {
                    app.busy = false;
                    app.status = "compaction aborted".into();
                    app.set_activity(Activity::Paused);
                    app.set_notice("compaction aborted", NoticeLevel::Warning);
                    app.push_transcript_line(Line_::System("compaction aborted".into()));
                }
                Err(error) => {
                    app.busy = false;
                    app.status = "compaction failed".into();
                    app.set_activity(Activity::Error);
                    let message = format!("compaction failed: {error:#}");
                    app.set_notice(&message, NoticeLevel::Error);
                    app.push_transcript_line(Line_::System(message));
                }
            }
            runtime.end_turn();
            let state = runtime.observe(app);
            return match queue_step(&state, completed) {
                QueueStep::Send => vec![Intent::SendQueued],
                QueueStep::Idle | QueueStep::Hold(_) => Vec::new(),
            };
        }
        Completion::Crashed(error) => {
            app.busy = false;
            // A crash delivers no TurnDone and no error event, so this
            // is the only place the transcript gets closed out.
            app.close_open_rows();
            app.status = "error".into();
            app.set_activity(Activity::Error);
            let message = format!("operation crashed: {error}");
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

/// A delivery to another session finished: file its outcome. Nothing
/// here touches the turn slot or the root's busy state — the delivery
/// never owned either.
fn routed_complete<R: Runtime>(
    app: &mut App,
    result: anyhow::Result<RouteOutcome>,
    notification: Notification,
    runtime: &mut R,
) {
    match result {
        Ok(RouteOutcome::Propagate(propagated)) => runtime.hold_propagate(propagated),
        Ok(RouteOutcome::Requeue(requeued)) => {
            app.set_persistent_notice(
                "notification paused; send a message to resume",
                NoticeLevel::Warning,
            );
            if !runtime.observe(app).turn_running {
                app.status = "notification paused; send a message to resume".into();
                app.set_activity(Activity::Paused);
            }
            runtime.hold_requeue(requeued);
            runtime.pause_notifications();
        }
        Ok(RouteOutcome::Complete) => {
            app.set_notice(
                format!(
                    "task result delivered to {}",
                    notification.parent_session_id
                ),
                NoticeLevel::Info,
            );
        }
        Err(error) => {
            // The delivery failed, but the child's final word is
            // right here: salvage it into the transcript instead of
            // losing the work with the plumbing error.
            let message = format!(
                "a task result could not be delivered to {}: {error:#}",
                notification.parent_session_id
            );
            app.set_notice(&message, NoticeLevel::Error);
            app.push_transcript_line(Line_::System(message));
            app.push_transcript_line(Line_::System(format!(
                "undelivered result of {}:\n{}",
                notification.description, notification.text
            )));
        }
    }
}

/// One settle pass, in the order that defines the schedule. The order
/// is the point: the gate must observe a turn the drain just started,
/// or a background notification steals the turn a queued message was
/// promised — the recorded queue-inversion bug.
pub(crate) fn settle<R: Runtime>(
    app: &mut App,
    intents: Vec<Intent>,
    runtime: &mut R,
) -> anyhow::Result<()> {
    for intent in intents {
        runtime.perform(app, intent)?;
    }
    runtime.peek_palette(app)?;
    if app.compact_requested && !runtime.observe(app).turn_running {
        app.compact_requested = false;
        runtime.start_compaction(app);
    }
    // An aside runs beside whatever else is happening — read-only, no
    // turn slot, no gate.
    if let Some(question) = app.aside_requested.take() {
        runtime.start_aside(app, question);
    }
    // The notification drain. Foreign completions resume other
    // sessions: they need nothing the root holds — not the turn slot,
    // not the keyboard — so they route immediately, even mid-turn,
    // even under a modal. Only a same-session completion wants the
    // turn slot, and only the first can have it; the rest are held in
    // arrival order. The requeue pause holds everything: it exists so
    // a failing delivery is retried when the user is back, not in a
    // tight loop.
    let state = runtime.observe(app);
    if !state.notifications_paused {
        let mut turn_gate = may_start_notification_turn(&state);
        let mut blocked = Vec::new();
        while let Some(notification) = runtime.next_notification() {
            if notification.parent_session_id != runtime.session_id() {
                runtime.route(app, notification);
            } else if turn_gate {
                runtime.start_notification_turn(app, notification);
                turn_gate = false;
            } else {
                blocked.push(notification);
            }
        }
        if !blocked.is_empty() {
            runtime.hold_blocked(blocked);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::waiting_texts;
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
            if let Some(request) = crate::apply_intent(app, intent, None) {
                self.turn_running = true;
                match request {
                    crate::TurnRequest::New(text, _) => self.log.push(format!("start_turn:{text}")),
                    crate::TurnRequest::Resume => self.log.push("resume_turn".into()),
                }
            }
            Ok(())
        }

        fn peek_palette(&mut self, _app: &mut App) -> anyhow::Result<()> {
            Ok(())
        }

        fn next_notification(&mut self) -> Option<Notification> {
            self.pending.pop_front()
        }

        fn start_compaction(&mut self, app: &mut App) {
            self.turn_running = true;
            app.busy = true;
            self.log.push("start_compaction".into());
        }

        fn start_aside(&mut self, _app: &mut App, question: String) {
            // Detached: neither the turn slot nor busy is touched.
            self.log.push(format!("start_aside:{question}"));
        }

        fn route(&mut self, _app: &mut App, notification: Notification) {
            // Detached: the turn slot is not touched.
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

        fn hold_blocked(&mut self, notifications: Vec<Notification>) {
            // Silent, like the real one: holding is not an event.
            for notification in notifications.into_iter().rev() {
                self.pending.push_front(notification);
            }
        }

        fn end_turn(&mut self) {
            self.log.push("end_turn".into());
        }

        fn revert_model(&mut self, app: &mut App, model: String, variant: Option<String>) {
            self.log.push(format!("revert:{model}"));
            app.current_model = model;
            app.current_variant = variant;
        }

        async fn start_subtask(&mut self, _app: &mut App, request: crate::app::SubtaskRequest) {
            self.log.push(format!("subtask:{}", request.description));
        }

        fn present(&mut self, _app: &mut App) -> anyhow::Result<()> {
            self.log.push("present".into());
            Ok(())
        }

        fn poll_event(&mut self, _busy: bool) -> anyhow::Result<Option<crossterm::event::Event>> {
            self.log.push("poll".into());
            Ok(None)
        }
    }

    #[test]
    fn manual_compaction_outranks_a_waiting_notification() {
        let mut app = App::new();
        app.compact_requested = true;
        let mut runtime = FakeRuntime::new().with_notification("root", "task finished");

        settle(&mut app, Vec::new(), &mut runtime).unwrap();

        assert_eq!(runtime.log, vec!["start_compaction"]);
        assert_eq!(runtime.pending.len(), 1, "the notification must wait");
    }

    #[test]
    fn compact_slash_starts_maintenance_without_a_model_turn() {
        let mut app = App::new();
        app.queued_messages = vec!["/compact".into()];
        let mut runtime = FakeRuntime::new();

        settle(&mut app, vec![Intent::SendQueued], &mut runtime).unwrap();

        assert_eq!(runtime.log, vec!["start_compaction"]);
        assert!(app.queued_messages.is_empty());
        assert!(
            app.lines()
                .iter()
                .all(|line| !matches!(line, Line_::User(_))),
            "/compact leaked into the transcript"
        );
    }

    #[test]
    fn compaction_completion_shows_summary_and_resumes_the_queue_only() {
        let mut app = App::new();
        app.busy = true;
        app.goal = Some(("ship it".into(), 3));
        app.queued_messages = vec!["wait for me".into()];
        let mut runtime = FakeRuntime::new();

        pass(
            &mut app,
            vec![Completion::Compaction(Ok(
                ManualCompactionOutcome::Compacted {
                    summary: "handover keeps the migration plan".into(),
                    context_tokens: 42,
                },
            ))],
            Vec::new(),
            &mut runtime,
        )
        .unwrap();

        assert_eq!(runtime.log, vec!["end_turn", "start_turn:wait for me"]);
        assert_eq!(app.goal, Some(("ship it".into(), 3)));
        assert!(app.queued_messages.is_empty());
        assert!(app.lines().iter().any(
            |line| matches!(line, Line_::System(text) if text.contains("handover keeps the migration plan"))
        ));
    }

    #[test]
    fn an_aside_starts_even_mid_turn_and_touches_nothing() {
        let mut app = App::new();
        app.busy = true;
        app.queued_messages = vec!["typed earlier".into()];
        let mut runtime = FakeRuntime::new();
        runtime.turn_running = true;

        app.aside_requested = Some("which port?".into());
        settle(&mut app, Vec::new(), &mut runtime).unwrap();

        assert_eq!(runtime.log, vec!["start_aside:which port?"]);
        assert_eq!(app.aside_requested, None);
        // The aside borrowed nothing: the turn keeps running, the
        // queue keeps waiting, busy stays whose it was.
        assert!(app.busy);
        assert_eq!(waiting_texts(&app.queued_messages), vec!["typed earlier"]);
    }

    #[test]
    fn aborted_compaction_holds_messages_queued_during_it() {
        let mut app = App::new();
        app.busy = true;
        app.queued_messages = vec!["wait for me".into()];
        let mut runtime = FakeRuntime::new();

        pass(
            &mut app,
            vec![Completion::Compaction(Ok(ManualCompactionOutcome::Aborted))],
            Vec::new(),
            &mut runtime,
        )
        .unwrap();

        assert_eq!(runtime.log, vec!["end_turn"]);
        assert_eq!(waiting_texts(&app.queued_messages), vec!["wait for me"]);
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

        settle(&mut app, vec![Intent::SendQueued], &mut runtime).unwrap();

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
        assert_eq!(waiting_texts(&app.queued_messages), vec!["held message"]);
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

        pass(
            &mut app,
            vec![Completion::Root(Ok(TurnOutcome::Completed))],
            Vec::new(),
            &mut runtime,
        )
        .unwrap();

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
            vec![Completion::Root(Ok(TurnOutcome::Completed))],
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
            vec![Completion::Root(Ok(TurnOutcome::Aborted))],
            Vec::new(),
            &mut runtime,
        )
        .unwrap();
        assert_eq!(runtime.log, vec!["end_turn"]);
        assert!(runtime.paused, "an abort must not resume notifications");
        assert_eq!(waiting_texts(&app.queued_messages), vec!["held"]);
        assert_eq!(runtime.pending.len(), 1);

        // The next completed turn resumes, and the gate opens in the
        // same pass. (Queue emptied: a queued message would outrank.)
        app.busy = true;
        app.queued_messages.clear();
        runtime.log.clear();
        pass(
            &mut app,
            vec![Completion::Root(Ok(TurnOutcome::Completed))],
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
            vec![Completion::Root(Ok(TurnOutcome::Completed))],
            Vec::new(),
            &mut runtime,
        )
        .unwrap();

        assert_eq!(runtime.log, vec!["end_turn"], "nothing starts");
        assert_eq!(waiting_texts(&app.queued_messages), vec!["go left"]);
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
            vec![Completion::Root(Ok(TurnOutcome::Completed))],
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
            vec![Completion::Routed {
                result: Ok(RouteOutcome::Requeue(notification.clone())),
                notification,
            }],
            Vec::new(),
            &mut runtime,
        )
        .unwrap();

        assert!(runtime.paused);
        assert_eq!(
            runtime.log,
            vec!["requeue:needs the user"],
            "a delivery ends no turn"
        );
        assert_eq!(runtime.pending.len(), 1, "held, not delivered");
    }

    /// The frame is drawn after the drain and before the poll: what
    /// the user clicks on next is the frame that reflects the turn
    /// that just started. Swapping present and poll fails this.
    #[tokio::test]
    async fn a_tick_draws_after_the_drain_and_before_the_poll() {
        let mut app = App::new();
        app.busy = true;
        app.queued_messages = vec!["next".into()];
        let mut runtime = FakeRuntime::new();

        let outcome = tick(
            &mut app,
            vec![Completion::Root(Ok(TurnOutcome::Completed))],
            Vec::new(),
            &mut runtime,
        )
        .await
        .unwrap();

        assert_eq!(outcome, Tick::Idle);
        assert_eq!(
            runtime.log,
            vec!["end_turn", "start_turn:next", "present", "poll"]
        );
    }

    /// A foreign notification routes inside the tick and the tick
    /// still draws: the delivery is detached, so nothing waits on it.
    #[tokio::test]
    async fn a_foreign_notification_routes_and_the_tick_still_draws() {
        let mut app = App::new();
        let mut runtime = FakeRuntime::new().with_notification("elsewhere", "done");

        let outcome = tick(&mut app, Vec::new(), Vec::new(), &mut runtime)
            .await
            .unwrap();

        assert_eq!(outcome, Tick::Idle);
        assert_eq!(runtime.log, vec!["route:elsewhere", "present", "poll"]);
    }

    /// A subtask armed during the drain spawns in the same tick,
    /// between the drain and the frame — and before a restart could
    /// defer it.
    #[tokio::test]
    async fn a_subtask_spawns_between_the_drain_and_the_frame() {
        let mut app = App::new();
        app.pending_subtask = Some(crate::app::SubtaskRequest {
            description: "/scout".into(),
            prompt: "look around".into(),
            agent: "explore".into(),
            model: None,
            variant: None,
        });
        let mut runtime = FakeRuntime::new();

        tick(&mut app, Vec::new(), Vec::new(), &mut runtime)
            .await
            .unwrap();

        assert_eq!(runtime.log, vec!["subtask:/scout", "present", "poll"]);
        assert!(app.pending_subtask.is_none());
    }

    /// An intent decided by the event half survives to the next tick's
    /// drain — the cross-iteration seam, driven as the loop drives it.
    #[tokio::test]
    async fn an_event_half_intent_drains_on_the_next_tick() {
        let mut app = App::new();
        let carried = crate::decide::submit(
            &FakeRuntime::new().observe(&app),
            false,
            "typed while idle".into(),
        );
        let mut runtime = FakeRuntime::new();

        tick(&mut app, Vec::new(), carried, &mut runtime).await.unwrap();

        assert_eq!(
            runtime.log,
            vec!["start_turn:typed while idle", "present", "poll"]
        );
    }

    /// A crashed turn task delivers no `TurnDone` and no error event,
    /// so the transcript it left mid-flight is the pass's to close:
    /// otherwise an idle app keeps spinning over work that is gone.
    #[test]
    fn a_crash_closes_what_the_turn_left_open() {
        use crate::transcript::ToolState;
        let mut app = App::new();
        app.busy = true;
        app.session_id = "root".into();
        app.push_loop_event(&ilar::agent::LoopEvent::ToolStarted {
            id: "call-1".into(),
            name: "task".into(),
        });
        app.push_subagent_activity(&ilar::subagent::SubagentActivity {
            parent_session_id: "root".into(),
            parent_call_id: "call-1".into(),
            child_session_id: "child".into(),
            agent: "explore".into(),
            event: ilar::agent::LoopEvent::TurnStarted,
        });
        app.push_loop_event(&ilar::agent::LoopEvent::ThinkingDelta("half a thou".into()));
        let mut runtime = FakeRuntime::new();

        pass(
            &mut app,
            vec![Completion::Crashed("turn task panicked".into())],
            Vec::new(),
            &mut runtime,
        )
        .unwrap();

        assert!(!app.busy);
        assert!(
            app.lines().iter().all(|line| !matches!(
                line,
                Line_::Thought {
                    complete: false,
                    ..
                }
            )),
            "an incomplete thought survived the crash: {:?}",
            app.lines()
        );
        let Some(Line_::Tool {
            state,
            child_running,
            ..
        }) = app
            .lines()
            .iter()
            .find(|line| matches!(line, Line_::Tool { .. }))
        else {
            panic!("{:?}", app.lines());
        };
        assert_eq!(*state, ToolState::Failed);
        assert!(!child_running, "the agent row still claims to be working");
    }

    /// An idle pass lets the notification through: same-session starts
    /// its turn here, foreign spawns its detached delivery.
    #[test]
    fn an_idle_pass_admits_the_notification() {
        let mut app = App::new();
        let mut runtime = FakeRuntime::new().with_notification("root", "task finished");
        settle(&mut app, Vec::new(), &mut runtime).unwrap();
        assert_eq!(runtime.log, vec!["notify_turn:task finished"]);

        let mut app = App::new();
        let mut runtime = FakeRuntime::new().with_notification("elsewhere", "done");
        settle(&mut app, Vec::new(), &mut runtime).unwrap();
        assert_eq!(runtime.log, vec!["route:elsewhere"]);
    }

    /// The requeue pause holds foreign notifications too — it is the
    /// only thing standing between a requeued delivery and a tight
    /// route-fail-requeue loop. Routing foreign past the pause fails
    /// this.
    #[test]
    fn a_paused_gate_holds_foreign_notifications() {
        let mut app = App::new();
        let mut runtime = FakeRuntime::new().with_notification("elsewhere", "done");
        runtime.paused = true;

        settle(&mut app, Vec::new(), &mut runtime).unwrap();

        assert!(runtime.log.is_empty(), "{:?}", runtime.log);
        assert_eq!(runtime.pending.len(), 1, "held for resume, not consumed");
    }

    /// The heart of the rework: a foreign completion routes even while
    /// a root turn runs and a modal owns the keyboard. Its delivery
    /// needs nothing the root holds, so nothing gates it — the very
    /// gate that once queued every completion behind a finished turn.
    #[test]
    fn a_foreign_completion_routes_while_a_turn_runs_and_a_modal_is_open() {
        let mut app = App::new();
        app.busy = true;
        app.help_visible = true;
        let mut runtime = FakeRuntime::new().with_notification("elsewhere", "done");
        runtime.turn_running = true;

        settle(&mut app, Vec::new(), &mut runtime).unwrap();

        assert_eq!(runtime.log, vec!["route:elsewhere"]);
        assert!(app.busy, "the root turn's state is untouched");
    }

    /// A same-session completion blocked by a running turn must not
    /// starve a foreign one behind it: the foreign delivery routes,
    /// the blocked one is held — in order, not lost.
    #[test]
    fn a_blocked_same_session_head_does_not_starve_a_foreign_delivery() {
        let mut app = App::new();
        let mut runtime = FakeRuntime::new()
            .with_notification("root", "for the root")
            .with_notification("elsewhere", "for a child");
        runtime.turn_running = true;

        settle(&mut app, Vec::new(), &mut runtime).unwrap();

        assert_eq!(runtime.log, vec!["route:elsewhere"]);
        assert_eq!(
            runtime
                .pending
                .iter()
                .map(|n| n.text.as_str())
                .collect::<Vec<_>>(),
            vec!["for the root"],
            "the blocked completion is held, not lost"
        );
    }

    /// A delivery finishing beside a live root turn files its outcome
    /// and touches nothing of the turn's: no end_turn, no channels
    /// torn down, no busy flip. Running Routed through the turn
    /// teardown fails this.
    #[test]
    fn a_delivery_completion_ends_no_turn() {
        let mut app = App::new();
        app.busy = true;
        let mut runtime = FakeRuntime::new();
        runtime.turn_running = true;
        let notification = Notification {
            parent_session_id: "child".into(),
            description: "background task".into(),
            text: "done".into(),
            is_error: false,
        };

        pass(
            &mut app,
            vec![Completion::Routed {
                result: Ok(RouteOutcome::Complete),
                notification,
            }],
            Vec::new(),
            &mut runtime,
        )
        .unwrap();

        assert!(runtime.log.is_empty(), "{:?}", runtime.log);
        assert!(app.busy, "the running turn's busy state survives");
    }

    /// Two deliveries propagating in one pass must both survive. The
    /// old single-slot hold overwrote the first with the second — a
    /// completion silently lost.
    #[test]
    fn a_second_propagate_does_not_overwrite_the_first() {
        let mut app = App::new();
        let mut runtime = FakeRuntime::new();
        runtime.turn_running = true;
        let propagated = |text: &str| Notification {
            parent_session_id: "root".into(),
            description: "nested task".into(),
            text: text.into(),
            is_error: false,
        };

        pass(
            &mut app,
            vec![
                Completion::Routed {
                    result: Ok(RouteOutcome::Propagate(propagated("first"))),
                    notification: propagated("first"),
                },
                Completion::Routed {
                    result: Ok(RouteOutcome::Propagate(propagated("second"))),
                    notification: propagated("second"),
                },
            ],
            Vec::new(),
            &mut runtime,
        )
        .unwrap();

        assert_eq!(
            runtime
                .pending
                .iter()
                .map(|n| n.text.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second"],
            "both held, in order"
        );
    }

    /// A delivery that fails outright still puts the child's final
    /// word in the transcript: the plumbing error must not take the
    /// work down with it.
    #[test]
    fn a_failed_delivery_salvages_the_result_into_the_transcript() {
        let mut app = App::new();
        let mut runtime = FakeRuntime::new();
        let notification = Notification {
            parent_session_id: "child".into(),
            description: "builder task".into(),
            text: "the build is green".into(),
            is_error: false,
        };

        pass(
            &mut app,
            vec![Completion::Routed {
                result: Err(anyhow::anyhow!("unknown persisted agent")),
                notification,
            }],
            Vec::new(),
            &mut runtime,
        )
        .unwrap();

        assert!(
            app.lines().iter().any(
                |line| matches!(line, Line_::System(text) if text.contains("the build is green"))
            ),
            "{:?}",
            app.lines()
        );
        assert!(app.lines().iter().any(
            |line| matches!(line, Line_::System(text) if text.contains("could not be delivered"))
        ));
    }
}
