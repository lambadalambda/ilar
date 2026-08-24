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
use crate::decide::{Intent, LoopState, QueueStep, after_turn, may_route_notification, queue_step};
use crate::transcript::Line_;
use crate::{Activity, MAX_GOAL_ROUNDS, NoticeLevel};
use ilar::agent::TurnOutcome;
use ilar::compaction::ManualCompactionOutcome;
use ilar::subagent::{Notification, RouteOutcome};

/// How the operation that was running ended — the edge hands this in after
/// awaiting the join, so the pass itself never blocks.
pub(crate) enum Completion {
    Root(anyhow::Result<TurnOutcome>),
    Routed(anyhow::Result<RouteOutcome>),
    Compaction(anyhow::Result<ManualCompactionOutcome>),
    /// A `/btw` question came back; `Ok(None)` means it was aborted.
    Aside {
        question: String,
        result: anyhow::Result<Option<String>>,
    },
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
    /// Start an explicitly requested idle-session compaction.
    fn start_compaction(&mut self, app: &mut App);
    /// Ask a `/btw` question over the session, off the record.
    fn start_aside(&mut self, app: &mut App, question: String);
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

/// What the caller does after a settle pass.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Settled {
    Proceed,
    /// A routing turn was spawned; restart the iteration so its
    /// completion is awaited before anything else happens.
    Restart,
}

/// How a tick ended, and what the caller owes the iteration.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Tick {
    /// A routing turn was spawned; restart without drawing so its
    /// completion is awaited before anything else happens.
    Restart,
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
    completion: Option<Completion>,
    carried: Vec<Intent>,
    runtime: &mut R,
) -> anyhow::Result<Tick> {
    let settled = pass(app, completion, carried, runtime)?;
    // After the drain — a queued command may have armed it there —
    // and before the restart, so a routed notification cannot defer
    // the spawn.
    if let Some(request) = app.pending_subtask.take() {
        runtime.start_subtask(app, request).await;
    }
    if settled == Settled::Restart {
        return Ok(Tick::Restart);
    }
    runtime.present(app)?;
    match runtime.poll_event(app.busy)? {
        Some(event) => Ok(Tick::Dispatch(event)),
        None => Ok(Tick::Idle),
    }
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
        Completion::Aside { question, result } => {
            // Same queue rule as compaction: an answered aside releases
            // messages queued behind it; an abort or failure holds them
            // for the user to reconsider.
            let completed = matches!(&result, Ok(Some(_)));
            let mut answered = None;
            match result {
                Ok(Some(answer)) => {
                    app.busy = false;
                    app.status = "ready".into();
                    app.set_activity(Activity::Ready);
                    answered = Some(answer);
                }
                Ok(None) => {
                    app.busy = false;
                    app.status = "aside aborted".into();
                    app.set_activity(Activity::Ready);
                    app.set_notice("aside aborted", NoticeLevel::Warning);
                }
                Err(error) => {
                    app.busy = false;
                    app.status = "aside failed".into();
                    app.set_activity(Activity::Error);
                    app.set_notice(format!("aside failed: {error:#}"), NoticeLevel::Error);
                }
            }
            runtime.end_turn();
            // The queue decision is taken *before* the modal opens: a
            // message queued during the aside was promised a turn, and
            // the answer is read-only — it can float above a streaming
            // turn. Deciding after would hold the message behind the
            // modal with nothing left to release it.
            let state = runtime.observe(app);
            let step = queue_step(&state, completed);
            if let Some(answer) = answered {
                app.aside = Some(crate::modals::AsideModal {
                    question,
                    answer,
                    scroll: 0,
                });
            }
            return match step {
                QueueStep::Send => vec![Intent::SendQueued],
                QueueStep::Idle | QueueStep::Hold(_) => Vec::new(),
            };
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
    if app.compact_requested && !runtime.observe(app).turn_running {
        app.compact_requested = false;
        runtime.start_compaction(app);
    }
    if app.aside_requested.is_some() && !runtime.observe(app).turn_running {
        let question = app.aside_requested.take().expect("checked above");
        runtime.start_aside(app, question);
    }
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
            if let Some(request) = crate::apply_intent(app, intent, None) {
                self.turn_running = true;
                match request {
                    crate::TurnRequest::New(text) => self.log.push(format!("start_turn:{text}")),
                    crate::TurnRequest::Resume => self.log.push("resume_turn".into()),
                }
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

        fn start_compaction(&mut self, app: &mut App) {
            self.turn_running = true;
            app.busy = true;
            self.log.push("start_compaction".into());
        }

        fn start_aside(&mut self, app: &mut App, question: String) {
            self.turn_running = true;
            app.busy = true;
            self.log.push(format!("start_aside:{question}"));
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
            app.lines.iter().all(|line| !matches!(line, Line_::User(_))),
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
            Some(Completion::Compaction(Ok(
                ManualCompactionOutcome::Compacted {
                    summary: "handover keeps the migration plan".into(),
                    context_tokens: 42,
                },
            ))),
            Vec::new(),
            &mut runtime,
        )
        .unwrap();

        assert_eq!(runtime.log, vec!["end_turn", "start_turn:wait for me"]);
        assert_eq!(app.goal, Some(("ship it".into(), 3)));
        assert!(app.queued_messages.is_empty());
        assert!(app.lines.iter().any(
            |line| matches!(line, Line_::System(text) if text.contains("handover keeps the migration plan"))
        ));
    }

    #[test]
    fn an_idle_aside_request_starts_and_the_answer_opens_the_modal() {
        let mut app = App::new();
        app.aside_requested = Some("which port?".into());
        let mut runtime = FakeRuntime::new();

        settle(&mut app, Vec::new(), &mut runtime).unwrap();
        assert_eq!(runtime.log, vec!["start_aside:which port?"]);
        assert_eq!(app.aside_requested, None);

        app.queued_messages = vec!["typed during it".into()];
        runtime.turn_running = false;
        pass(
            &mut app,
            Some(Completion::Aside {
                question: "which port?".into(),
                result: Ok(Some("Port 8080, behind nginx.".into())),
            }),
            Vec::new(),
            &mut runtime,
        )
        .unwrap();

        let aside = app.aside.as_ref().expect("modal opened");
        assert_eq!(aside.question, "which port?");
        assert_eq!(aside.answer, "Port 8080, behind nginx.");
        // The queued message got the turn it was promised — released
        // before the modal opened, which would otherwise hold it with
        // nothing left to let go.
        assert_eq!(
            runtime.log,
            vec![
                "start_aside:which port?",
                "end_turn",
                "start_turn:typed during it"
            ]
        );
        assert!(app.queued_messages.is_empty(), "queued message stranded");
        // The released turn shows in the transcript; the aside itself
        // does not — the exchange lives only in the modal.
        let rendered = format!("{:?}", app.lines);
        assert!(!rendered.contains("which port?"), "{rendered}");
        assert!(!rendered.contains("8080"), "{rendered}");
    }

    #[test]
    fn a_failed_aside_is_a_notice_not_a_modal() {
        let mut app = App::new();
        app.busy = true;
        let mut runtime = FakeRuntime::new();

        pass(
            &mut app,
            Some(Completion::Aside {
                question: "anything?".into(),
                result: Err(anyhow::anyhow!("provider melted")),
            }),
            Vec::new(),
            &mut runtime,
        )
        .unwrap();

        assert!(app.aside.is_none());
        assert!(!app.busy);
        assert!(app.status.contains("failed"), "{}", app.status);
    }

    #[test]
    fn aborted_compaction_holds_messages_queued_during_it() {
        let mut app = App::new();
        app.busy = true;
        app.queued_messages = vec!["wait for me".into()];
        let mut runtime = FakeRuntime::new();

        pass(
            &mut app,
            Some(Completion::Compaction(Ok(ManualCompactionOutcome::Aborted))),
            Vec::new(),
            &mut runtime,
        )
        .unwrap();

        assert_eq!(runtime.log, vec!["end_turn"]);
        assert_eq!(app.queued_messages, vec!["wait for me"]);
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
            Some(Completion::Root(Ok(TurnOutcome::Completed))),
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

    /// A routed notification restarts the iteration without drawing —
    /// its completion is awaited first, exactly as the old `continue`
    /// behaved.
    #[tokio::test]
    async fn a_restart_skips_the_frame_and_the_poll() {
        let mut app = App::new();
        let mut runtime = FakeRuntime::new().with_notification("elsewhere", "done");

        let outcome = tick(&mut app, None, Vec::new(), &mut runtime)
            .await
            .unwrap();

        assert_eq!(outcome, Tick::Restart);
        assert_eq!(runtime.log, vec!["route:elsewhere"]);
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

        tick(&mut app, None, Vec::new(), &mut runtime)
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

        tick(&mut app, None, carried, &mut runtime).await.unwrap();

        assert_eq!(
            runtime.log,
            vec!["start_turn:typed while idle", "present", "poll"]
        );
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
