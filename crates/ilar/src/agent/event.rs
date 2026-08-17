//! Events the loop publishes for the UI (and tests).

#[derive(Debug, Clone)]
pub enum LoopEvent {
    TurnStarted,
    TextDelta(String),
    ThinkingDelta(String),
    ToolStarted {
        id: String,
        name: String,
    },
    ToolArguments {
        id: String,
        arguments: String,
    },
    ToolFinished {
        id: String,
        name: String,
        is_error: bool,
    },
    /// One provider call completed (stop reason + usage).
    StepComplete {
        stop_reason: String,
        usage: crate::session::Usage,
    },
    /// The transcript was compacted before this turn's provider call.
    Compacted {
        context_tokens: u64,
    },
    TurnDone {
        outcome: crate::agent::TurnOutcome,
    },
}

pub const LOOP_EVENT_CAPACITY: usize = 64;
const MAX_COALESCED_DELTA_BYTES: usize = 16 * 1024;

pub struct LoopEventSender {
    sender: tokio::sync::mpsc::Sender<LoopEvent>,
    terminal: Option<tokio::sync::mpsc::OwnedPermit<LoopEvent>>,
}

pub struct LoopEventReceiver {
    receiver: tokio::sync::mpsc::Receiver<LoopEvent>,
    pending: Option<LoopEvent>,
}

/// Bounded loop-event channel with one additional slot reserved for `TurnDone`.
pub fn loop_event_channel(capacity: usize) -> (LoopEventSender, LoopEventReceiver) {
    let (sender, receiver) = tokio::sync::mpsc::channel(capacity.max(1) + 1);
    let terminal = sender
        .clone()
        .try_reserve_owned()
        .expect("new loop event channel has terminal capacity");
    (
        LoopEventSender {
            sender,
            terminal: Some(terminal),
        },
        LoopEventReceiver {
            receiver,
            pending: None,
        },
    )
}

impl LoopEventSender {
    /// Publish in FIFO order, abandoning a capacity wait when the turn is cancelled.
    pub async fn publish(&self, event: LoopEvent, cancel: &tokio_util::sync::CancellationToken) {
        tokio::select! {
            biased;
            () = cancel.cancelled() => {}
            _ = self.sender.send(event) => {}
        }
    }

    /// Publish the single terminal event through capacity reserved at construction.
    pub fn publish_terminal(&mut self, event: LoopEvent) {
        if let Some(permit) = self.terminal.take() {
            let _ = permit.send(event);
        }
    }
}

impl LoopEventReceiver {
    pub async fn recv(&mut self) -> Option<LoopEvent> {
        let event = match self.pending.take() {
            Some(event) => event,
            None => self.receiver.recv().await?,
        };
        Some(self.coalesce_available(event))
    }

    pub fn try_recv(&mut self) -> Result<LoopEvent, tokio::sync::mpsc::error::TryRecvError> {
        let event = match self.pending.take() {
            Some(event) => event,
            None => self.receiver.try_recv()?,
        };
        Ok(self.coalesce_available(event))
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.receiver.len() + usize::from(self.pending.is_some())
    }

    fn coalesce_available(&mut self, mut event: LoopEvent) -> LoopEvent {
        loop {
            let is_text = match &event {
                LoopEvent::TextDelta(_) => true,
                LoopEvent::ThinkingDelta(_) => false,
                _ => return event,
            };
            let next = match self.receiver.try_recv() {
                Ok(next) => next,
                Err(_) => return event,
            };
            let adjacent = match (is_text, &next) {
                (true, LoopEvent::TextDelta(next)) | (false, LoopEvent::ThinkingDelta(next)) => {
                    Some(next)
                }
                _ => None,
            };
            let text = match &mut event {
                LoopEvent::TextDelta(text) | LoopEvent::ThinkingDelta(text) => text,
                _ => unreachable!(),
            };
            if let Some(next) = adjacent
                && text.len().saturating_add(next.len()) <= MAX_COALESCED_DELTA_BYTES
            {
                text.push_str(next);
            } else {
                self.pending = Some(next);
                return event;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::TurnOutcome;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn bounded_channel_reserves_terminal_capacity_and_cancels_blocked_sends() {
        let (mut tx, mut rx) = loop_event_channel(1);
        let cancel = CancellationToken::new();
        tx.publish(LoopEvent::TextDelta("first".into()), &cancel)
            .await;
        assert_eq!(rx.len(), 1);

        let task_cancel = cancel.clone();
        let blocked = tokio::spawn(async move {
            tx.publish(LoopEvent::TextDelta("second".into()), &task_cancel)
                .await;
            tx.publish_terminal(LoopEvent::TurnDone {
                outcome: TurnOutcome::Aborted,
            });
        });
        tokio::task::yield_now().await;
        assert!(!blocked.is_finished());
        cancel.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(1), blocked)
            .await
            .expect("cancellation releases blocked publisher")
            .unwrap();

        assert!(matches!(rx.recv().await, Some(LoopEvent::TextDelta(text)) if text == "first"));
        assert!(matches!(
            rx.recv().await,
            Some(LoopEvent::TurnDone {
                outcome: TurnOutcome::Aborted
            })
        ));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn receiver_coalesces_adjacent_deltas_without_crossing_boundaries() {
        let (tx, mut rx) = loop_event_channel(4);
        let cancel = CancellationToken::new();
        tx.publish(LoopEvent::TextDelta("hel".into()), &cancel)
            .await;
        tx.publish(LoopEvent::TextDelta("lo".into()), &cancel).await;
        tx.publish(LoopEvent::ThinkingDelta("one".into()), &cancel)
            .await;
        tx.publish(LoopEvent::ThinkingDelta(" two".into()), &cancel)
            .await;

        assert!(matches!(rx.recv().await, Some(LoopEvent::TextDelta(text)) if text == "hello"));
        assert!(
            matches!(rx.recv().await, Some(LoopEvent::ThinkingDelta(text)) if text == "one two")
        );
    }

    #[tokio::test]
    async fn receiver_caps_coalesced_delta_size() {
        let (tx, mut rx) = loop_event_channel(2);
        let cancel = CancellationToken::new();
        let chunk = "x".repeat(MAX_COALESCED_DELTA_BYTES / 2 + 1);
        tx.publish(LoopEvent::TextDelta(chunk.clone()), &cancel)
            .await;
        tx.publish(LoopEvent::TextDelta(chunk.clone()), &cancel)
            .await;

        assert!(matches!(rx.recv().await, Some(LoopEvent::TextDelta(text)) if text == chunk));
        assert!(matches!(rx.recv().await, Some(LoopEvent::TextDelta(text)) if text == chunk));
    }
}
