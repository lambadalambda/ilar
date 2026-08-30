//! What "delivered" means, and what to do when a delivery ends.
//!
//! A background child's completion is routed to the session that
//! spawned it, and every surface that can drive a session has to agree
//! about two things: whether a notification already arrived, and what
//! each ending of a delivery attempt obliges the driver to do. Both
//! answers used to live wherever a driver happened to need them — the
//! delivered check in three hand-written copies, the endings as prose
//! in one driver's completion handler — and the second driver quietly
//! grew a shorter list of obligations than the first: no retire, no
//! salvage, so a terminally undeliverable result was re-announced and
//! re-failed at every start, forever.
//!
//! So both live here. The predicate is one function. The endings are
//! an enum a driver must match exhaustively, which is the point: a
//! driver that forgets to salvage now fails to compile instead of
//! quietly losing a child's work.

use crate::session::{SessionEvent, SessionReader};
use crate::subagent::{Notification, RouteOutcome};

/// Whether this text already sits in a `UserMessage` of the target's
/// log.
///
/// The log is the one artifact every process shares, so it is the only
/// honest answer to "did this arrive?" — a second ilar may have
/// delivered it, this process may have delivered it before a crash.
/// `contains` rather than equality because a delivering prompt can
/// carry queued steers ahead of the notification text.
///
/// Accepted limitation, the same one the outbox compaction documents:
/// two byte-identical notification texts for one parent dedupe as one.
pub fn is_delivered(parent: &SessionReader, text: &str) -> bool {
    delivered_in(parent.events(), text)
}

/// [`is_delivered`] against an event slice.
fn delivered_in(events: &[SessionEvent], text: &str) -> bool {
    events.iter().any(|event| match event {
        SessionEvent::UserMessage {
            text: appended, ..
        } => appended.contains(text),
        _ => false,
    })
}

/// How many times one completion may climb the parent chain before a
/// driver stops carrying it. Ancestry is a tree of single-digit depth,
/// so eight hops is unreachable by nesting — the budget exists for a
/// corrupted or hand-edited pair of logs that name each other as
/// parents, the same cycle `outbox::pending`'s ancestry cap refuses to
/// walk forever.
pub const PROPAGATION_HOPS: usize = 8;

/// One notification in flight with the climb budget it has left.
///
/// The budget is the parcel's, not the notification's: the same
/// completion may be delivered, propagated to a parent, and held again,
/// and only the climbing is bounded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parcel {
    notification: Notification,
    hops: usize,
}

impl Parcel {
    /// A notification straight off the channel: a full budget.
    pub fn fresh(notification: Notification) -> Self {
        Self {
            notification,
            hops: PROPAGATION_HOPS,
        }
    }

    pub fn notification(&self) -> &Notification {
        &self.notification
    }

    pub fn into_notification(self) -> Notification {
        self.notification
    }

    /// The same budget carrying a different notification — a hold or a
    /// requeue, which is not a climb.
    fn carrying(&self, notification: Notification) -> Self {
        Self {
            notification,
            hops: self.hops,
        }
    }

    /// One hop poorer, carrying the notification the climb is for.
    /// `Err` when the budget is spent — and it hands that notification
    /// *back*, because it is the one now stranded: the parcel's own was
    /// appended to the log of the session this hop just reached, while
    /// this one was freshly recorded for a session nothing can reach.
    /// Losing it here would leave an outbox entry nobody retires, to be
    /// re-adopted with a full budget at the next start.
    pub fn climbing(&self, notification: Notification) -> Result<Self, Notification> {
        match self.hops.checked_sub(1) {
            Some(hops) => Ok(Self {
                notification,
                hops,
            }),
            None => Err(notification),
        }
    }
}

/// What a finished delivery attempt obliges its driver to do.
///
/// Exhaustive on purpose: a driver that handles all but one of these
/// loses work in the one it forgot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disposition {
    /// It arrived. Say so; there is nothing to keep.
    Delivered,
    /// The target belongs to another tree — hand the notification up to
    /// whoever owns that tree. Still undelivered, still owed, and one
    /// hop poorer.
    Propagate(Parcel),
    /// It has climbed as far as it may. Only a parent chain that loops
    /// gets here, and the work in the text is real, so this ends the
    /// same way a terminal failure does rather than in silence.
    Exhausted(Notification),
    /// The target could not take it *yet* (its writer is held, its
    /// session is mid-turn elsewhere). Hold it and stop announcing new
    /// ones until something moves, or the next attempt races the same
    /// wall.
    Hold(Parcel),
    /// The attempt failed in a way retrying will not fix. The child's
    /// work is in the notification text, so the driver must put it
    /// somewhere a human will see it *and* retire the outbox entry —
    /// that salvage is the delivery of last resort, and without the
    /// retire the next start announces, re-attempts and re-fails it
    /// forever.
    Salvage {
        notification: Notification,
        error: String,
    },
}

/// Read a delivery attempt's ending. The one place the mapping lives,
/// so two drivers cannot disagree about what a `Requeue` owes.
pub fn disposition(result: anyhow::Result<RouteOutcome>, parcel: Parcel) -> Disposition {
    match result {
        Ok(RouteOutcome::Complete) => Disposition::Delivered,
        Ok(RouteOutcome::Propagate(propagated)) => match parcel.climbing(propagated) {
            Ok(next) => Disposition::Propagate(next),
            Err(stranded) => Disposition::Exhausted(stranded),
        },
        Ok(RouteOutcome::Requeue(requeued)) => Disposition::Hold(parcel.carrying(requeued)),
        Err(error) => Disposition::Salvage {
            notification: parcel.into_notification(),
            error: format!("{error:#}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SessionMeta, SessionStore, new_id};

    fn notification(text: &str) -> Notification {
        Notification {
            parent_session_id: "parent".into(),
            description: "a survey".into(),
            text: text.into(),
            is_error: false,
        }
    }

    /// The delivering prompt can carry queued steers ahead of the
    /// notification, so the question is containment, not equality —
    /// and a *different* completion's arrival is not this one's.
    #[test]
    fn delivery_is_the_text_appearing_in_a_prompt() {
        let events = vec![SessionEvent::UserMessage {
            id: new_id(),
            text: "look at this first\n\n<task-notification>\ndone\n</task-notification>".into(),
            images: Vec::new(),
            ts: chrono::Utc::now(),
        }];
        assert!(delivered_in(
            &events,
            "<task-notification>\ndone\n</task-notification>"
        ));
        assert!(!delivered_in(
            &events,
            "<task-notification>\nother\n</task-notification>"
        ));
        assert!(!delivered_in(&[], "anything"));
    }

    /// Only a prompt counts. A child's completion quoted back by the
    /// model, or salvaged into an assistant turn, was not delivered to
    /// it.
    #[test]
    fn only_a_user_message_counts_as_delivery() {
        let events = vec![SessionEvent::AssistantMessage {
            id: new_id(),
            model: "test/model".into(),
            content: vec![crate::session::ContentBlock::Text {
                text: "the task said: done".into(),
            }],
            usage: crate::session::Usage::default(),
            stop_reason: "end_turn".into(),
            ts: chrono::Utc::now(),
        }];
        assert!(!delivered_in(&events, "done"));
    }

    #[test]
    fn a_reader_answers_the_same_as_its_events() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let session_id = new_id();
        let mut session = store
            .create(SessionMeta {
                session_id: session_id.clone(),
                parent_id: None,
                agent: "build".into(),
                model: "test/model".into(),
                workspace: None,
                cwd: None,
            })
            .unwrap();
        session
            .append(SessionEvent::UserMessage {
                id: new_id(),
                text: "the completion".into(),
                images: Vec::new(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(session);

        let reader = store.load(&session_id).unwrap();
        assert!(is_delivered(&reader, "the completion"));
        assert!(!is_delivered(&reader, "another completion"));
    }

    /// The mapping every driver folds. Pinned as a whole, because the
    /// bug this module exists to prevent was a driver that read three
    /// of these four correctly.
    #[test]
    fn every_ending_names_what_it_owes() {
        let parcel = || Parcel::fresh(notification("done"));
        assert_eq!(
            disposition(Ok(RouteOutcome::Complete), parcel()),
            Disposition::Delivered
        );
        assert_eq!(
            disposition(Ok(RouteOutcome::Propagate(notification("up"))), parcel()),
            Disposition::Propagate(Parcel {
                notification: notification("up"),
                hops: PROPAGATION_HOPS - 1,
            })
        );
        // A hold is not a climb: the budget is untouched, or a
        // notification that bounced a few times would run out of room
        // to reach the parent that can take it.
        assert_eq!(
            disposition(Ok(RouteOutcome::Requeue(notification("later"))), parcel()),
            Disposition::Hold(Parcel {
                notification: notification("later"),
                hops: PROPAGATION_HOPS,
            })
        );
        let Disposition::Salvage {
            notification: salvaged,
            error,
        } = disposition(
            Err(anyhow::anyhow!("the writer is gone").context("delivering")),
            parcel(),
        ) else {
            panic!("a failed delivery must be salvaged, never dropped");
        };
        assert_eq!(salvaged, notification("done"));
        // The whole chain, so the salvage line says what went wrong.
        assert!(error.contains("delivering"), "{error}");
        assert!(error.contains("the writer is gone"), "{error}");
    }

    /// A parent chain that loops would otherwise keep a completion
    /// climbing forever. The budget runs out, and what it was carrying
    /// is surfaced rather than dropped — the text is a finished child's
    /// only word.
    #[test]
    fn a_climb_that_never_arrives_runs_out_and_says_so() {
        let mut parcel = Parcel::fresh(notification("hop 0"));
        for hop in 1..=PROPAGATION_HOPS {
            let next = notification(&format!("hop {hop}"));
            match disposition(Ok(RouteOutcome::Propagate(next)), parcel) {
                Disposition::Propagate(carried) => parcel = carried,
                other => panic!("the budget ended early at hop {hop}: {other:?}"),
            }
        }

        // Every hop carries a *different* notification, so this pins
        // which one is surfaced: the freshly recorded one that is now
        // stranded, not the one the previous hop already appended to a
        // log — retiring that one would tombstone the wrong file and
        // leave the real entry to be re-adopted forever.
        assert_eq!(
            disposition(Ok(RouteOutcome::Propagate(notification("stranded"))), parcel),
            Disposition::Exhausted(notification("stranded"))
        );
    }
}
