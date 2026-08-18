//! Scripted provider for TDD of the loop, executor and future consumers.

use std::sync::{Arc, Mutex};

use futures::stream;

use super::event::ProviderEvent;
use super::request::Request;
use super::{EventStream, Provider};

/// Serves scripted turns through the [`Provider`] trait. Turns play in
/// order and unexpected calls fail unless repetition is explicitly enabled.
#[derive(Clone, Default)]
pub struct MockProvider {
    state: Arc<Mutex<MockState>>,
    repeat_last: bool,
}

#[derive(Default)]
struct MockState {
    turns: Vec<Vec<ProviderEvent>>,
    requests: Vec<Request>,
}

impl MockProvider {
    pub fn new(turns: Vec<Vec<ProviderEvent>>) -> Self {
        Self {
            state: Arc::new(Mutex::new(MockState {
                turns,
                requests: Vec::new(),
            })),
            repeat_last: false,
        }
    }

    /// Scripted provider whose final turn repeats indefinitely.
    pub fn repeating(turns: Vec<Vec<ProviderEvent>>) -> Self {
        Self {
            repeat_last: true,
            ..Self::new(turns)
        }
    }

    /// Provider that immediately errors.
    pub fn error(message: impl Into<String>) -> Self {
        Self::new(vec![vec![ProviderEvent::Error(message.into())]])
    }

    /// All requests seen so far, for assertions.
    pub fn requests(&self) -> Vec<Request> {
        self.state.lock().unwrap().requests.clone()
    }
}

impl Provider for MockProvider {
    fn stream(&self, req: Request) -> anyhow::Result<EventStream> {
        let mut state = self.state.lock().unwrap();
        state.requests.push(req);
        let events = match state.turns.len() {
            0 => anyhow::bail!("mock provider script exhausted"),
            1 if self.repeat_last => state.turns[0].clone(),
            _ => state.turns.remove(0),
        };
        Ok(Box::pin(stream::iter(events)))
    }
}
