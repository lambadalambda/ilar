//! Events the loop publishes for the UI (and tests).

#[derive(Debug, Clone)]
pub enum LoopEvent {
    TurnStarted,
    TextDelta(String),
    ThinkingDelta(String),
    ReasoningSummaryDelta(String),
    ReasoningSummaryCompleted,
    ToolStarted {
        id: String,
        name: String,
    },
    ToolArguments {
        id: String,
        arguments: String,
    },
    ToolInputProgress {
        id: String,
        received_bytes: u64,
        last_data: std::time::Instant,
    },
    ToolInputComplete {
        id: String,
    },
    SubagentConfigured {
        id: String,
        description: String,
        agent: String,
    },
    ToolExecutionStarted {
        id: String,
        received_bytes: u64,
        started: std::time::Instant,
    },
    ToolExecutionCompleted {
        id: String,
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
    progress:
        std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, ToolProgressSnapshot>>>,
    progress_wake: tokio::sync::mpsc::Sender<()>,
}

pub struct LoopEventReceiver {
    receiver: tokio::sync::mpsc::Receiver<LoopEvent>,
    pending: Option<LoopEvent>,
    progress:
        std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, ToolProgressSnapshot>>>,
    progress_wake: tokio::sync::mpsc::Receiver<()>,
    ready_progress: std::collections::VecDeque<LoopEvent>,
}

#[derive(Clone, Copy)]
struct ToolProgressSnapshot {
    received_bytes: u64,
    last_data: std::time::Instant,
}

/// Bounded loop-event channel with one additional slot reserved for `TurnDone`.
pub fn loop_event_channel(capacity: usize) -> (LoopEventSender, LoopEventReceiver) {
    let (sender, receiver) = tokio::sync::mpsc::channel(capacity.max(1) + 1);
    let terminal = sender
        .clone()
        .try_reserve_owned()
        .expect("new loop event channel has terminal capacity");
    let progress = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let (progress_wake, progress_receiver) = tokio::sync::mpsc::channel(1);
    (
        LoopEventSender {
            sender,
            terminal: Some(terminal),
            progress: progress.clone(),
            progress_wake,
        },
        LoopEventReceiver {
            receiver,
            pending: None,
            progress,
            progress_wake: progress_receiver,
            ready_progress: std::collections::VecDeque::new(),
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

    /// Publish a cumulative progress snapshot without slowing the provider stream.
    pub fn publish_tool_input_progress(&self, id: &str, received_bytes: u64) {
        self.progress.lock().unwrap().insert(
            id.to_string(),
            ToolProgressSnapshot {
                received_bytes,
                last_data: std::time::Instant::now(),
            },
        );
        let _ = self.progress_wake.try_send(());
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
        loop {
            match self.try_recv() {
                Ok(event) => return Some(event),
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => return None,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
            }
            tokio::select! {
                biased;
                event = self.receiver.recv() => {
                    return event.map(|event| self.coalesce_available(event));
                }
                wake = self.progress_wake.recv() => {
                    wake?;
                    self.drain_progress();
                }
            }
        }
    }

    pub fn try_recv(&mut self) -> Result<LoopEvent, tokio::sync::mpsc::error::TryRecvError> {
        let reliable = match self.pending.take() {
            Some(event) => event,
            None => match self.receiver.try_recv() {
                Ok(event) => event,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    if let Some(progress) = self.next_progress() {
                        return Ok(progress);
                    }
                    return Err(tokio::sync::mpsc::error::TryRecvError::Empty);
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    if let Some(progress) = self.next_progress() {
                        return Ok(progress);
                    }
                    return Err(tokio::sync::mpsc::error::TryRecvError::Disconnected);
                }
            },
        };
        Ok(self.coalesce_available(reliable))
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.receiver.len()
            + usize::from(self.pending.is_some())
            + self.progress_wake.len()
            + self.ready_progress.len()
    }

    fn coalesce_available(&mut self, mut event: LoopEvent) -> LoopEvent {
        loop {
            let delta_kind = match &event {
                LoopEvent::TextDelta(_) => 0,
                LoopEvent::ThinkingDelta(_) => 1,
                LoopEvent::ReasoningSummaryDelta(_) => 2,
                _ => return event,
            };
            let next = match self.receiver.try_recv() {
                Ok(next) => next,
                Err(_) => return event,
            };
            let adjacent = match (delta_kind, &next) {
                (0, LoopEvent::TextDelta(next))
                | (1, LoopEvent::ThinkingDelta(next))
                | (2, LoopEvent::ReasoningSummaryDelta(next)) => Some(next),
                _ => None,
            };
            let text = match &mut event {
                LoopEvent::TextDelta(text)
                | LoopEvent::ThinkingDelta(text)
                | LoopEvent::ReasoningSummaryDelta(text) => text,
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

    fn next_progress(&mut self) -> Option<LoopEvent> {
        if let Some(progress) = self.ready_progress.pop_front() {
            return Some(progress);
        }
        self.progress_wake.try_recv().ok()?;
        self.drain_progress();
        self.ready_progress.pop_front()
    }

    fn drain_progress(&mut self) {
        while self.progress_wake.try_recv().is_ok() {}
        let updates = std::mem::take(&mut *self.progress.lock().unwrap());
        let mut updates = updates.into_iter().collect::<Vec<_>>();
        updates.sort_by(|(left, _), (right, _)| left.cmp(right));
        self.ready_progress.extend(updates.into_iter().map(
            |(
                id,
                ToolProgressSnapshot {
                    received_bytes,
                    last_data,
                },
            )| {
                LoopEvent::ToolInputProgress {
                    id,
                    received_bytes,
                    last_data,
                }
            },
        ));
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
        let (tx, mut rx) = loop_event_channel(8);
        let cancel = CancellationToken::new();
        tx.publish(LoopEvent::TextDelta("hel".into()), &cancel)
            .await;
        tx.publish(LoopEvent::TextDelta("lo".into()), &cancel).await;
        tx.publish(LoopEvent::ThinkingDelta("one".into()), &cancel)
            .await;
        tx.publish(LoopEvent::ThinkingDelta(" two".into()), &cancel)
            .await;
        tx.publish(
            LoopEvent::ReasoningSummaryDelta("**Running".into()),
            &cancel,
        )
        .await;
        tx.publish(LoopEvent::ReasoningSummaryDelta(" tests**".into()), &cancel)
            .await;
        tx.publish(LoopEvent::ReasoningSummaryCompleted, &cancel)
            .await;

        assert!(matches!(rx.recv().await, Some(LoopEvent::TextDelta(text)) if text == "hello"));
        assert!(
            matches!(rx.recv().await, Some(LoopEvent::ThinkingDelta(text)) if text == "one two")
        );
        assert!(matches!(
            rx.recv().await,
            Some(LoopEvent::ReasoningSummaryDelta(text)) if text == "**Running tests**"
        ));
        assert!(matches!(
            rx.recv().await,
            Some(LoopEvent::ReasoningSummaryCompleted)
        ));
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

    #[tokio::test]
    async fn tool_progress_is_lossy_and_coalesces_to_the_latest_count() {
        let (tx, mut rx) = loop_event_channel(3);
        tx.publish_tool_input_progress("write-1", 1024);
        tx.publish_tool_input_progress("write-1", 4096);
        tx.publish_tool_input_progress("write-1", 8192);
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let Some(LoopEvent::ToolInputProgress {
            id,
            received_bytes,
            last_data,
        }) = rx.recv().await
        else {
            panic!("expected tool progress");
        };
        assert_eq!(id, "write-1");
        assert_eq!(received_bytes, 8192);
        assert!(last_data.elapsed() >= std::time::Duration::from_millis(10));

        tx.publish_tool_input_progress("write-1", 16_384);
        tx.publish_tool_input_progress("write-1", 32_768);
        tx.publish_tool_input_progress("write-1", 65_536);
        tx.publish_tool_input_progress("write-1", 131_072);
        assert_eq!(rx.len(), 1, "progress uses one latest-value wakeup");
    }

    #[tokio::test]
    async fn tool_progress_does_not_consume_reliable_event_capacity() {
        let (tx, mut rx) = loop_event_channel(1);
        let cancel = CancellationToken::new();
        tx.publish_tool_input_progress("write-1", 1024);

        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            tx.publish(LoopEvent::TurnStarted, &cancel),
        )
        .await
        .expect("progress blocked a reliable lifecycle event");
        assert!(matches!(rx.recv().await, Some(LoopEvent::TurnStarted)));
        assert!(matches!(
            rx.recv().await,
            Some(LoopEvent::ToolInputProgress { id, .. }) if id == "write-1"
        ));
    }
}
