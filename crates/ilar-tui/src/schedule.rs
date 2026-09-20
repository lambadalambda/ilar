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
use ilar::delivery::{Disposition, Parcel};
use ilar::subagent::{Notification, RouteOutcome};

/// What the warm-cache compaction says on its way in, and what it
/// leaves standing afterwards. Both used to point at `/rewind`, which
/// is the one thing that would make it worse: every rewind cut is a
/// user message, so the newest one discards the last answer and
/// reverts its edits. Nothing is actually lost — the model's own
/// `history` tool reads the whole log, summarized turns included — so
/// that is what they point at.
pub(crate) const CACHE_COMPACTING_LINE: &str = "compacting while the provider cache is still warm — nothing is lost; \
     the full log stays searchable (the agent's history tool)";
pub(crate) const CACHE_COMPACTED_NOTICE: &str = "compacted automatically to keep the provider cache warm — nothing is lost; \
     the full log stays searchable (the agent's history tool)";

/// How the operation that was running ended — the edge hands this in after
/// awaiting the join, so the pass itself never blocks.
pub(crate) enum Completion {
    Root(anyhow::Result<TurnOutcome>),
    /// A detached delivery to another session finished. Carries the
    /// parcel it was delivering — the notification, so a failure can
    /// still put the child's final word in front of the user instead of
    /// losing it with the plumbing error, and the climb budget it had
    /// left.
    Routed {
        result: anyhow::Result<RouteOutcome>,
        parcel: Parcel,
        /// Someone cancelled this delivery — cancel-all, or a quit.
        /// A cancelled delivery requeues, which looks exactly like a
        /// busy target from the outside; only the token knows the
        /// difference, and the held notice must say which it was.
        cancelled: bool,
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
    fn next_notification(&mut self) -> Option<Parcel>;
    /// Start an explicitly requested idle-session compaction.
    fn start_compaction(&mut self, app: &mut App);
    /// Ask a `/btw` question over the session, off the record.
    fn start_aside(&mut self, app: &mut App, question: String);
    /// A delivery failed terminally and its text was just salvaged
    /// into the transcript — the delivery of last resort. Record that
    /// with the durable outbox so the next session open does not
    /// announce and re-attempt the same entry forever. Never called
    /// for transient (Requeue) outcomes: those hold and retry.
    fn retire_notification(&mut self, notification: &Notification);
    /// Write a salvaged result into *this* session's log, the way a
    /// delivered one would have been written into its own. The
    /// transcript lines the salvage pushes live in memory, and the
    /// outbox entry is retired the moment it is salvaged, so without
    /// this the child's final word lasts until the session is closed
    /// and no longer. Answers whether it landed: another writer holds
    /// the lease while a turn runs, and the user is told which of the
    /// two happened rather than promised the better one.
    fn record_salvage(&mut self, notification: &Notification) -> bool;
    /// A notification for another session: spawn its delivery beside
    /// whatever else is running. It resumes a child, so it takes
    /// neither the turn slot nor the keyboard, and several may run.
    fn route(&mut self, app: &mut App, parcel: Parcel);
    /// A notification for this session: start its turn here.
    fn start_notification_turn(&mut self, app: &mut App, notification: Notification);
    /// A notification for this session while its turn runs: send it
    /// into that turn like any other steer. Handed back when there is
    /// no live channel — the caller holds it instead.
    fn steer_notification(&mut self, app: &mut App, parcel: Parcel) -> Option<Parcel>;
    fn session_id(&self) -> &str;
    /// A session by name rather than id — "this session", a roster
    /// row's agent and task, or the persisted agent and opening prompt
    /// — for every message that says where a result went.
    fn session_label(&mut self, app: &App, session_id: &str) -> String;
    /// A turn ended without an abort: notifications flow again.
    fn resume_notifications(&mut self);
    /// A routed notification asked to wait for the user.
    fn pause_notifications(&mut self);
    /// A routed notification came back for this session: hold it
    /// behind whatever was already queued ahead of it.
    fn hold_propagate(&mut self, parcel: Parcel);
    /// A requeued notification goes to the front, to be re-offered as
    /// soon as the user resumes.
    fn hold_requeue(&mut self, parcel: Parcel);
    /// Same-session notifications the gate could not admit this pass,
    /// in arrival order: put them back in front so nothing is lost
    /// and order holds.
    fn hold_blocked(&mut self, parcels: Vec<Parcel>);
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
            parcel,
            cancelled,
        } => {
            routed_complete(app, result, parcel, cancelled, runtime);
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
                    if std::mem::take(&mut app.auto_compaction) {
                        // Standing: nobody was watching when it ran.
                        app.set_persistent_notice(CACHE_COMPACTED_NOTICE, NoticeLevel::Info);
                    } else {
                        app.set_notice("compaction complete", NoticeLevel::Info);
                    }
                }
                Ok(ManualCompactionOutcome::NothingToCompact) => {
                    app.auto_compaction = false;
                    app.busy = false;
                    app.status = "ready".into();
                    app.set_activity(Activity::Ready);
                    app.set_notice("nothing to compact", NoticeLevel::Info);
                }
                Ok(ManualCompactionOutcome::Aborted) => {
                    app.auto_compaction = false;
                    app.busy = false;
                    app.status = "compaction aborted".into();
                    // The same state an aborted turn leaves: `■
                    // compaction aborted`, not a paused `Ⅱ` that reads
                    // as work still waiting to go on.
                    app.set_activity(Activity::Aborted);
                    app.push_transcript_line(Line_::System("compaction aborted".into()));
                }
                Err(error) => {
                    app.auto_compaction = false;
                    app.busy = false;
                    app.status = "compaction failed".into();
                    app.set_activity(Activity::Error);
                    let message = format!("compaction failed: {error:#}");
                    app.set_persistent_notice(&message, NoticeLevel::Error);
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
            app.set_persistent_notice(&message, NoticeLevel::Error);
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

/// What every surface says to do about a held result: `deliver`, not
/// `retry`, and the two ways to. The status line uses this verbatim;
/// `view.rs` prefixes a count and `hold_notice` a reason.
pub(crate) const HELD_RESULT_ACTION: &str = "send a message, or Ctrl-Q to deliver";

/// The status line a held result puts up.
pub(crate) const HELD_RESULT_STATUS: &str = "task result held — send a message to deliver";

/// What a held delivery says on the notice line. A delivery the user
/// just cancelled says *cancelled* — and says it of the delivery, not
/// the result, which survives and still goes out. The "cannot reach it
/// while it is busy" wording otherwise landed on the notice line
/// moments after cancel-all, overwriting "background tasks cancelled"
/// with a complaint about a wall nobody hit.
fn hold_notice(target: &str, cancelled: bool) -> String {
    let reason = if cancelled {
        format!("the delivery of a task result for {target} was cancelled")
    } else {
        format!("a task result for {target} arrived while it was busy")
    };
    format!("{reason} — held; {HELD_RESULT_ACTION}")
}

/// The transcript's one-line receipt for a delivery that moved: `✉
/// "survey the API" delivered to explore · land the fix`. Quiet, in
/// the transcript rather than the notice line, because it asks the
/// user for nothing.
fn envelope_line(description: &str, verb: &str, target: &str) -> String {
    format!("✉ \"{description}\" {verb} {target}")
}

/// A delivery to another session finished: file its outcome. Nothing
/// here touches the turn slot or the root's busy state — the delivery
/// never owned either.
fn routed_complete<R: Runtime>(
    app: &mut App,
    result: anyhow::Result<RouteOutcome>,
    parcel: Parcel,
    cancelled: bool,
    runtime: &mut R,
) {
    // Kept for the success notice, which names what arrived where; the
    // disposition takes ownership of the parcel because the endings
    // that still owe something have to hold on to it.
    let delivered = parcel.notification().clone();
    // What each ending owes is `ilar::delivery`'s to say; this function
    // only knows how to say it on a terminal. The match is exhaustive
    // by construction, which is the point — the other driver of this
    // store grew a shorter list of obligations than this one by writing
    // its own.
    match ilar::delivery::disposition(result, parcel) {
        Disposition::Delivered => {
            let target = runtime.session_label(app, &delivered.parent_session_id);
            app.push_transcript_line(Line_::System(envelope_line(
                &delivered.description,
                "delivered to",
                &target,
            )));
        }
        Disposition::Propagate { parcel, retire } => {
            // The ✉ row this delivery wore is about to vanish, and the
            // next hop's own completion can be minutes away: say where
            // the result went, the way the landing says where it
            // landed.
            let next = runtime.session_label(app, &parcel.notification().parent_session_id);
            app.push_transcript_line(Line_::System(envelope_line(
                &delivered.description,
                "passed on to",
                &next,
            )));
            // Set only for a climb that replaced what it was carrying;
            // without the retire the next open adopts that entry and
            // fails it again. See `Disposition::Propagate`.
            if let Some(origin) = retire {
                runtime.retire_notification(&origin);
            }
            runtime.hold_propagate(parcel);
        }
        Disposition::Hold(requeued) => {
            let target = runtime.session_label(app, &requeued.notification().parent_session_id);
            app.set_persistent_notice(hold_notice(&target, cancelled), NoticeLevel::Warning);
            if !runtime.observe(app).turn_running {
                app.status = HELD_RESULT_STATUS.into();
                app.set_activity(Activity::Paused);
            }
            runtime.hold_requeue(requeued);
            runtime.pause_notifications();
        }
        Disposition::Exhausted {
            notification,
            retire,
        } => {
            // A parent chain that loops. Nothing can deliver this, so
            // it ends where a terminal failure ends: in front of the
            // user, and retired so the next open does not start the
            // same climb.
            let kept = runtime.record_salvage(&notification);
            let message = format!(
                "a task result could not find a session to land in after {} hops — its parent \
                 chain loops{}",
                ilar::delivery::PROPAGATION_HOPS,
                in_memory_only(kept)
            );
            app.set_notice(&message, NoticeLevel::Error);
            app.push_transcript_line(Line_::System(message));
            // The child's words, in the row every other surface gives
            // a notification: collapsed, expandable, not a wall of
            // raw `<task-notification>` envelope as a System line.
            app.push_notification(&notification.description, &notification.text);
            runtime.retire_notification(&notification);
            // The origin a replacing hop superseded on the way here is
            // owed its retire too — a spent budget is no excuse.
            if let Some(origin) = retire {
                runtime.retire_notification(&origin);
            }
        }
        Disposition::Salvage {
            notification,
            error,
        } => {
            // The delivery failed, but the child's final word is
            // right here: salvage it into the transcript instead of
            // losing the work with the plumbing error.
            let target = runtime.session_label(app, &notification.parent_session_id);
            // A delivery the user stopped is not a failure, and a
            // stopped one always ends here rather than held: the turn
            // it had already started appended the result, so replaying
            // it would deliver it twice. Say what happened, at the
            // level a cancel deserves.
            let kept = runtime.record_salvage(&notification);
            let (message, level) = if cancelled {
                (
                    format!(
                        "the delivery of a task result to {target} was cancelled{}",
                        in_memory_only(kept)
                    ),
                    NoticeLevel::Warning,
                )
            } else {
                (
                    format!(
                        "a task result could not be delivered to {target}: {error}{}",
                        in_memory_only(kept)
                    ),
                    NoticeLevel::Error,
                )
            };
            app.set_notice(&message, level);
            app.push_transcript_line(Line_::System(message));
            // The child's words, in the row every other surface gives
            // a notification: collapsed, expandable, not a wall of
            // raw `<task-notification>` envelope as a System line.
            app.push_notification(&notification.description, &notification.text);
            // The salvage above IS the delivery of last resort: retire
            // the outbox entry so the next open does not announce,
            // re-attempt and re-fail it forever.
            runtime.retire_notification(&notification);
        }
    }
}

/// What a salvage adds to its own message when the log would not take
/// it — a running turn holds the writer, or the session is parked on a
/// question. The text is in front of the user either way; only how long
/// it lasts differs, and that is worth a clause rather than a line.
fn in_memory_only(kept: bool) -> &'static str {
    if kept {
        ""
    } else {
        ". Its text is in this transcript only, and goes when the session closes"
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
    } else if crate::decide::cache_compact_due(
        &runtime.observe(app),
        &app.cache_compact_check(),
        std::time::Instant::now(),
    ) {
        // The provider still holds this context; a summary made now
        // reads it at cached rates and leaves a small one behind for
        // the cold read that is coming either way.
        app.cache_compact_fired = true;
        app.auto_compaction = true;
        app.push_transcript_line(Line_::System(CACHE_COMPACTING_LINE.into()));
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
    // even under a modal. A same-session completion starts a turn
    // when the slot is free, and is steered into the turn that is
    // running otherwise — results reach every agent, the root
    // included, as soon as possible. Held only when the turn is
    // unsteerable. The requeue pause holds everything: it exists so
    // a failing delivery is retried when the user is back, not in a
    // tight loop.
    let state = runtime.observe(app);
    if !state.notifications_paused {
        let mut turn_gate = may_start_notification_turn(&state);
        let mut blocked = Vec::new();
        while let Some(parcel) = runtime.next_notification() {
            if parcel.notification().parent_session_id != runtime.session_id() {
                runtime.route(app, parcel);
            } else if turn_gate {
                // A turn here ends the climb: the completion arrived
                // where it was addressed.
                runtime.start_notification_turn(app, parcel.into_notification());
                // The slot is taken — but the turn it now runs is
                // steerable, so the rest of a burst lands inside it.
                turn_gate = false;
            } else if let Some(unsteered) = runtime.steer_notification(app, parcel) {
                blocked.push(unsteered);
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

    /// The child's word reached the transcript, whatever row it wears:
    /// a collapsed notification row for a real envelope, a user row
    /// for a bare text. Losing it is the failure the salvage tests
    /// guard; which row it lands in is the renderer's business.
    fn transcript_carries(app: &App, needle: &str) -> bool {
        app.lines().iter().any(|line| match line {
            Line_::System(text)
            | Line_::User(text)
            | Line_::Task { text, .. }
            | Line_::Job { text, .. } => text.contains(needle),
            _ => false,
        })
    }

    /// Mirrors the real runtime's shape: `perform` goes through the
    /// real `apply_intent`, and starting any turn flips `turn_running`
    /// — the flag the gate reads. That flip is what a reordered
    /// schedule gets wrong.
    struct FakeRuntime {
        session_id: String,
        turn_running: bool,
        steerable: bool,
        paused: bool,
        pending: VecDeque<Parcel>,
        log: Vec<String>,
        /// Whether `record_salvage` gets the writer. False stands for a
        /// turn holding it.
        log_takes_salvage: bool,
    }

    impl FakeRuntime {
        fn new() -> Self {
            Self {
                session_id: "root".into(),
                turn_running: false,
                steerable: false,
                paused: false,
                pending: VecDeque::new(),
                log: Vec::new(),
                log_takes_salvage: true,
            }
        }

        fn with_notification(mut self, parent: &str, text: &str) -> Self {
            self.pending.push_back(Parcel::fresh(Notification {
                parent_session_id: parent.into(),
                description: "background task".into(),
                text: text.into(),
                is_error: false,
            }));
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
                retry_available: app.retry_available,
                model_key_pending: app.model_key_pending,
            }
        }

        fn perform(&mut self, app: &mut App, intent: Intent) -> anyhow::Result<()> {
            if let Some(request) = crate::apply_intent(app, intent, None) {
                self.turn_running = true;
                // A real user turn opens a steer channel.
                self.steerable = true;
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

        fn next_notification(&mut self) -> Option<Parcel> {
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

        fn retire_notification(&mut self, notification: &Notification) {
            self.log.push(format!("retire:{}", notification.text));
        }

        fn record_salvage(&mut self, notification: &Notification) -> bool {
            self.log.push(format!("record:{}", notification.text));
            self.log_takes_salvage
        }

        fn route(&mut self, _app: &mut App, parcel: Parcel) {
            // Detached: the turn slot is not touched.
            self.log
                .push(format!("route:{}", parcel.notification().parent_session_id));
        }

        fn session_label(&mut self, _app: &App, session_id: &str) -> String {
            format!("label of {session_id}")
        }

        fn start_notification_turn(&mut self, _app: &mut App, notification: Notification) {
            self.turn_running = true;
            // The freshly started turn is steerable, like the real one.
            self.steerable = true;
            self.log.push(format!("notify_turn:{}", notification.text));
        }

        fn steer_notification(&mut self, _app: &mut App, parcel: Parcel) -> Option<Parcel> {
            if !self.steerable {
                return Some(parcel);
            }
            self.log
                .push(format!("steer_notify:{}", parcel.notification().text));
            None
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

        fn hold_propagate(&mut self, parcel: Parcel) {
            self.log
                .push(format!("hold:{}", parcel.notification().text));
            self.pending.push_back(parcel);
        }

        fn hold_requeue(&mut self, parcel: Parcel) {
            self.log
                .push(format!("requeue:{}", parcel.notification().text));
            self.pending.push_front(parcel);
        }

        fn hold_blocked(&mut self, parcels: Vec<Parcel>) {
            // Silent, like the real one: holding is not an event.
            for parcel in parcels.into_iter().rev() {
                self.pending.push_front(parcel);
            }
        }

        fn end_turn(&mut self) {
            self.steerable = false;
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

    /// An idle session whose cache window is closing compacts once,
    /// says so in the transcript, and does not fire again until a turn
    /// resets the episode.
    #[test]
    fn the_warm_cache_compaction_fires_once_per_idle_episode() {
        let mut app = App::new();
        app.cache_compact.enabled = true;
        app.current_model = "openai/gpt-5.6-sol".into();
        app.context_used = 400_000;
        app.cache_idle_since =
            Some(std::time::Instant::now() - std::time::Duration::from_secs(3600));
        let mut runtime = FakeRuntime::new();

        settle(&mut app, Vec::new(), &mut runtime).unwrap();
        assert_eq!(runtime.log, vec!["start_compaction"]);
        assert!(app.auto_compaction);
        assert!(app.lines().iter().any(
            |line| matches!(line, Line_::System(text) if text.contains("cache is still warm"))
        ));

        // Once per episode: a second pass leaves it alone.
        runtime.turn_running = false;
        settle(&mut app, Vec::new(), &mut runtime).unwrap();
        assert_eq!(runtime.log, vec!["start_compaction"]);

        // Off by default: a fresh app never fires.
        let mut app = App::new();
        app.context_used = 400_000;
        app.cache_idle_since =
            Some(std::time::Instant::now() - std::time::Duration::from_secs(3600));
        let mut runtime = FakeRuntime::new();
        settle(&mut app, Vec::new(), &mut runtime).unwrap();
        assert!(runtime.log.is_empty(), "{:?}", runtime.log);
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
        // An abort is an abort: the status line reads `■ compaction
        // aborted`, not a paused `Ⅱ` that reads as work still to come.
        assert_eq!(app.activity, Activity::Aborted);
        assert_eq!(app.status, "compaction aborted");
    }

    /// A compaction must not send the user somewhere that makes it
    /// worse. `/rewind`'s newest cut drops the last answer and reverts
    /// its edits; the log is searchable and loses nothing.
    #[test]
    fn the_cache_compaction_never_points_at_rewind() {
        for text in [CACHE_COMPACTING_LINE, CACHE_COMPACTED_NOTICE] {
            assert!(!text.contains("rewind"), "{text}");
            assert!(text.contains("nothing is lost"), "{text}");
            assert!(text.contains("history"), "{text}");
        }

        let mut app = App::new();
        app.auto_compaction = true;
        let mut runtime = FakeRuntime::new();
        pass(
            &mut app,
            vec![Completion::Compaction(Ok(
                ManualCompactionOutcome::Compacted {
                    summary: "the story so far".into(),
                    context_tokens: 1_000,
                },
            ))],
            Vec::new(),
            &mut runtime,
        )
        .unwrap();
        let (notice, _) = app.operational_notice().expect("a standing notice");
        assert_eq!(notice, CACHE_COMPACTED_NOTICE);
    }

    /// The recorded queue-inversion bug, pinned: a turn completes with
    /// a message queued AND a notification waiting. The queued message
    /// must get the turn — the notification must never start a turn of
    /// its own ahead of it. It now rides the queued message's turn as
    /// a steer instead of waiting behind it.
    #[test]
    fn a_dequeued_message_outranks_a_notification_in_the_same_pass() {
        let mut app = App::new();
        app.queued_messages = vec!["do the next thing".into()];
        let mut runtime = FakeRuntime::new().with_notification("root", "task finished");

        settle(&mut app, vec![Intent::SendQueued], &mut runtime).unwrap();

        assert_eq!(
            runtime.log,
            vec!["start_turn:do the next thing", "steer_notify:task finished"],
            "the queued message gets the turn; the result rides it"
        );
        assert!(runtime.pending.is_empty());
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
            vec![
                "end_turn",
                "start_turn:do the next thing",
                "steer_notify:task finished"
            ]
        );
        assert!(runtime.pending.is_empty());
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
                parcel: Parcel::fresh(notification),
                cancelled: false,
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

        tick(&mut app, Vec::new(), carried, &mut runtime)
            .await
            .unwrap();

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

    /// The root gets its results the way every agent now does: a
    /// completion arriving mid-turn is steered into the running turn,
    /// not parked until the conversation pauses. Only an unsteerable
    /// turn holds it.
    #[test]
    fn a_same_session_notification_steers_into_the_live_turn() {
        let mut app = App::new();
        let mut runtime = FakeRuntime::new().with_notification("root", "task finished");
        runtime.turn_running = true;
        runtime.steerable = true;

        settle(&mut app, Vec::new(), &mut runtime).unwrap();

        assert_eq!(runtime.log, vec!["steer_notify:task finished"]);
        assert!(runtime.pending.is_empty(), "delivered, not held");
    }

    /// A burst while idle: the first completion takes the turn slot,
    /// and the rest steer into the turn it just started instead of
    /// waiting behind it.
    #[test]
    fn a_burst_starts_one_turn_and_steers_the_rest_into_it() {
        let mut app = App::new();
        let mut runtime = FakeRuntime::new()
            .with_notification("root", "first done")
            .with_notification("root", "second done");

        settle(&mut app, Vec::new(), &mut runtime).unwrap();

        assert_eq!(
            runtime.log,
            vec!["notify_turn:first done", "steer_notify:second done"]
        );
        assert!(runtime.pending.is_empty());
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
                .map(|parcel| parcel.notification().text.as_str())
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
                parcel: Parcel::fresh(notification),
                cancelled: false,
            }],
            Vec::new(),
            &mut runtime,
        )
        .unwrap();

        assert!(runtime.log.is_empty(), "{:?}", runtime.log);
        assert!(app.busy, "the running turn's busy state survives");
        // Where it went, by name, as a transcript line — the notice
        // line stays free for things the user must act on.
        assert_eq!(
            app.lines().last(),
            Some(&Line_::System(
                "✉ \"background task\" delivered to label of child".into()
            ))
        );
        assert!(app.notice_text().is_none(), "{:?}", app.notice_text());
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
                    parcel: Parcel::fresh(propagated("first")),
                    cancelled: false,
                },
                Completion::Routed {
                    result: Ok(RouteOutcome::Propagate(propagated("second"))),
                    parcel: Parcel::fresh(propagated("second")),
                    cancelled: false,
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
                .map(|parcel| parcel.notification().text.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second"],
            "both held, in order"
        );
    }

    /// A delivery that fails outright still puts the child's final
    /// word in the transcript: the plumbing error must not take the
    /// work down with it. And the salvage is the delivery of last
    /// resort — the outbox entry is retired, so the next session open
    /// does not announce and re-fail it forever.
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
                parcel: Parcel::fresh(notification),
                cancelled: false,
            }],
            Vec::new(),
            &mut runtime,
        )
        .unwrap();

        assert!(
            transcript_carries(&app, "the build is green"),
            "{:?}",
            app.lines()
        );
        assert!(app.lines().iter().any(
            |line| matches!(line, Line_::System(text) if text.contains("could not be delivered"))
        ));
        assert_eq!(
            runtime.log,
            vec!["record:the build is green", "retire:the build is green"],
            "the salvaged text is written to the log before its entry is retired"
        );
    }

    /// The salvage is the last copy of the child's work: the outbox
    /// entry is retired on the spot, and transcript lines are not
    /// persisted. It goes into this session's log, and when the log
    /// will not take it — a turn holds the writer — the message says
    /// so rather than implying the text is safe.
    #[test]
    fn a_salvaged_result_is_written_to_the_log_or_says_it_was_not() {
        let notification = || Notification {
            parent_session_id: "child".into(),
            description: "builder task".into(),
            text: "the build is green".into(),
            is_error: false,
        };
        let run = |takes: bool| {
            let mut app = App::new();
            let mut runtime = FakeRuntime::new();
            runtime.log_takes_salvage = takes;
            pass(
                &mut app,
                vec![Completion::Routed {
                    result: Err(anyhow::anyhow!("unknown persisted agent")),
                    parcel: Parcel::fresh(notification()),
                    cancelled: false,
                }],
                Vec::new(),
                &mut runtime,
            )
            .unwrap();
            app.lines()
                .iter()
                .filter_map(|line| match line {
                    Line_::System(text) if text.contains("could not be delivered") => {
                        Some(text.clone())
                    }
                    _ => None,
                })
                .next()
                .expect("the failure is said out loud")
        };

        let kept = run(true);
        assert!(
            !kept.contains("this transcript only"),
            "a log that took it promises nothing extra: {kept}"
        );
        let lost = run(false);
        assert!(
            lost.contains("this transcript only"),
            "a log that refused it must not be passed off as durable: {lost}"
        );
    }

    /// A propagation that never arrives runs out of climb, and what it
    /// was carrying is a finished child's only word: it ends where a
    /// terminal failure ends — in the transcript, and retired — rather
    /// than being dropped for the queue to re-announce forever. Only a
    /// parent chain that loops gets here, which is exactly the case
    /// nothing else guards.
    #[test]
    fn a_completion_that_climbs_forever_is_salvaged_at_the_last_hop() {
        let mut app = App::new();
        let mut runtime = FakeRuntime::new();
        let notification = |text: &str| Notification {
            parent_session_id: "a-loop".into(),
            description: "builder task".into(),
            text: text.into(),
            is_error: false,
        };
        // Spent by construction: the hop-by-hop drain is `delivery`'s
        // own test, and a pass here would re-route the held parcel
        // before this test could reach for it.
        let mut parcel = Parcel::fresh(notification("an earlier hop"));
        for _ in 0..ilar::delivery::PROPAGATION_HOPS {
            parcel = parcel
                .climbing(notification("an earlier hop"))
                .expect("the budget covers its own length");
        }

        pass(
            &mut app,
            vec![Completion::Routed {
                // The notification that has nowhere left to go is the
                // one this last attempt produced, not the one the
                // parcel arrived carrying.
                result: Ok(RouteOutcome::Propagate(notification("the build is green"))),
                parcel,
                cancelled: false,
            }],
            Vec::new(),
            &mut runtime,
        )
        .unwrap();

        assert!(
            transcript_carries(&app, "the build is green"),
            "the child's word was dropped with the climb: {:?}",
            app.lines()
        );
        assert!(
            runtime
                .log
                .contains(&"retire:the build is green".to_string()),
            "{:?}",
            runtime.log
        );
        assert!(
            runtime
                .log
                .contains(&"record:the build is green".to_string()),
            "a spent climb is as terminal as a failure: write it down too — {:?}",
            runtime.log
        );
    }

    /// A climb that replaced an origin nothing could take retires that
    /// origin as it passes the replacement on. Without it the entry
    /// stays undelivered for ever: every open adopts it, fails the same
    /// restore, and manufactures the same failure note for the root —
    /// six times for one root before this was fixed.
    #[test]
    fn a_propagate_that_replaces_its_origin_retires_it() {
        let mut app = App::new();
        let mut runtime = FakeRuntime::new();
        let origin = Notification {
            parent_session_id: "a-vanished-worktree".into(),
            description: "review the hub package".into(),
            text: "the grandchild's report".into(),
            is_error: false,
        };
        let replacement = Notification {
            parent_session_id: "root".into(),
            description: "review the hub package".into(),
            text: "its workspace could not be restored: the grandchild's report".into(),
            is_error: true,
        };

        pass(
            &mut app,
            vec![Completion::Routed {
                result: Ok(RouteOutcome::Replace(replacement.clone())),
                parcel: Parcel::fresh(origin.clone()),
                cancelled: false,
            }],
            Vec::new(),
            &mut runtime,
        )
        .unwrap();

        // The origin, not the replacement: retiring the replacement
        // would tombstone the wrong file and leave the real entry to be
        // re-adopted at the next open.
        assert!(
            runtime.log.contains(&format!("retire:{}", origin.text)),
            "{:?}",
            runtime.log
        );
        assert!(
            runtime.log.contains(&format!("hold:{}", replacement.text)),
            "{:?}",
            runtime.log
        );
        assert!(
            !runtime
                .log
                .contains(&format!("retire:{}", replacement.text)),
            "{:?}",
            runtime.log
        );
    }

    /// The ordinary climb has no origin to retire: the target's log took
    /// the notification and ran a turn on it, and that log is its
    /// retire. Retiring here would tombstone an entry whose delivery
    /// already counted, for no gain, and blur the distinction the
    /// replacing climb depends on.
    #[test]
    fn an_ordinary_propagate_retires_nothing() {
        let mut app = App::new();
        let mut runtime = FakeRuntime::new();
        let notification = |session: &str, text: &str| Notification {
            parent_session_id: session.into(),
            description: "nested task".into(),
            text: text.into(),
            is_error: false,
        };

        pass(
            &mut app,
            vec![Completion::Routed {
                result: Ok(RouteOutcome::Propagate(notification("root", "the result"))),
                parcel: Parcel::fresh(notification("child", "the child's own")),
                cancelled: false,
            }],
            Vec::new(),
            &mut runtime,
        )
        .unwrap();

        assert!(
            runtime
                .log
                .iter()
                .all(|entry| !entry.starts_with("retire:")),
            "{:?}",
            runtime.log
        );
    }

    /// The transient counterpart: a Requeue outcome holds and retries —
    /// it must never retire the outbox entry, or a delivery that only
    /// needed the user's return would be written off as undeliverable.
    #[test]
    fn a_requeued_routing_retires_nothing() {
        let mut app = App::new();
        let mut runtime = FakeRuntime::new();
        let notification = Notification {
            parent_session_id: "child".into(),
            description: "background task".into(),
            text: "needs the user".into(),
            is_error: false,
        };

        pass(
            &mut app,
            vec![Completion::Routed {
                result: Ok(RouteOutcome::Requeue(notification.clone())),
                parcel: Parcel::fresh(notification),
                cancelled: false,
            }],
            Vec::new(),
            &mut runtime,
        )
        .unwrap();

        assert!(
            runtime
                .log
                .iter()
                .all(|entry| !entry.starts_with("retire:")),
            "{:?}",
            runtime.log
        );
    }
}
