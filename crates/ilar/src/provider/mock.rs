//! Scripted provider for TDD of the loop, executor and future consumers.

use std::sync::{Arc, Mutex};

use futures::stream;

use super::event::ProviderEvent;
use super::request::Request;
use super::{EventStream, Provider};

/// Serves scripted turns through the [`Provider`] trait. Turns play in
/// order; after the script is exhausted the last turn repeats (keeps
/// loop tests simple).
#[derive(Clone, Default)]
pub struct MockProvider {
    turns: Arc<Mutex<Vec<Vec<ProviderEvent>>>>,
    requests: Arc<Mutex<Vec<Request>>>,
}

impl MockProvider {
    pub fn new(turns: Vec<Vec<ProviderEvent>>) -> Self {
        Self {
            turns: Arc::new(Mutex::new(turns)),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Provider that immediately errors.
    pub fn error(message: impl Into<String>) -> Self {
        Self::new(vec![vec![ProviderEvent::Error(message.into())]])
    }

    /// All requests seen so far, for assertions.
    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    fn next_turn(&self) -> Vec<ProviderEvent> {
        let mut turns = self.turns.lock().unwrap();
        match turns.len() {
            0 => vec![ProviderEvent::Error("mock: no scripted turns".into())],
            1 => turns[0].clone(),
            _ => turns.remove(0),
        }
    }
}

impl Provider for MockProvider {
    fn stream(&self, req: Request) -> anyhow::Result<EventStream> {
        self.requests.lock().unwrap().push(req);
        let events = self.next_turn();
        Ok(Box::pin(stream::iter(events)))
    }
}
