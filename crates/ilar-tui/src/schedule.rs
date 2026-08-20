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
use crate::decide::{Intent, LoopState, may_route_notification};
use ilar::subagent::Notification;

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
}

/// What the caller does after a settle pass.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Settled {
    Proceed,
    /// A routing turn was spawned; restart the iteration so its
    /// completion is awaited before anything else happens.
    Restart,
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
        pending: VecDeque<Notification>,
        log: Vec<String>,
    }

    impl FakeRuntime {
        fn new() -> Self {
            Self {
                session_id: "root".into(),
                turn_running: false,
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
                notifications_paused: false,
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
