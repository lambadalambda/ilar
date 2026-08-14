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
    Compacted,
    TurnDone {
        outcome: crate::agent::TurnOutcome,
    },
}

/// Convenience: publish helper used internally.
pub(crate) fn publish(tx: &tokio::sync::mpsc::UnboundedSender<LoopEvent>, event: LoopEvent) {
    // Unbounded: send never blocks; a dropped receiver is fine (headless).
    let _ = tx.send(event);
}
