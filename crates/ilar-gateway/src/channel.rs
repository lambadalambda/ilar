//! A channel: somewhere a person types, and somewhere the answer goes.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::bus::{Inbound, Outbound};

pub type ChannelFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait Channel: Send + Sync {
    fn name(&self) -> &str;

    /// Run until cancelled, publishing everything received.
    fn run<'a>(
        &'a self,
        inbound: mpsc::Sender<Inbound>,
        cancel: CancellationToken,
    ) -> ChannelFuture<'a, anyhow::Result<()>>;

    fn send<'a>(&'a self, message: Outbound) -> ChannelFuture<'a, anyhow::Result<()>>;
}

/// A channel with nobody behind it: tests inject what a person would
/// have typed and read what they would have seen.
pub struct FakeChannel {
    name: String,
    injector: mpsc::Sender<Inbound>,
    injected: Mutex<Option<mpsc::Receiver<Inbound>>>,
    sent: Mutex<Vec<Outbound>>,
    delivered: tokio::sync::Notify,
}

impl FakeChannel {
    pub fn new(name: &str) -> Arc<Self> {
        let (injector, injected) = mpsc::channel(64);
        Arc::new(Self {
            name: name.to_string(),
            injector,
            injected: Mutex::new(Some(injected)),
            sent: Mutex::new(Vec::new()),
            delivered: tokio::sync::Notify::new(),
        })
    }

    /// What a person typed. Buffered, so it may precede `run`.
    pub async fn inject(&self, text: &str, chat_id: &str, sender_id: &str) {
        self.injector
            .send(Inbound {
                channel: self.name.clone(),
                chat_id: chat_id.to_string(),
                sender_id: sender_id.to_string(),
                text: text.to_string(),
                media: Vec::new(),
                is_group: false,
            })
            .await
            .expect("fake channel receiver dropped");
    }

    pub fn sent(&self) -> Vec<Outbound> {
        self.sent.lock().unwrap().clone()
    }

    /// Wait until at least `count` messages went out, or the timeout.
    pub async fn wait_for_sent(&self, count: usize, timeout: std::time::Duration) -> Vec<Outbound> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let sent = self.sent();
            if sent.len() >= count {
                return sent;
            }
            if tokio::time::timeout_at(deadline, self.delivered.notified())
                .await
                .is_err()
            {
                return self.sent();
            }
        }
    }
}

impl Channel for FakeChannel {
    fn name(&self) -> &str {
        &self.name
    }

    fn run<'a>(
        &'a self,
        inbound: mpsc::Sender<Inbound>,
        cancel: CancellationToken,
    ) -> ChannelFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let Some(mut injected) = self.injected.lock().unwrap().take() else {
                anyhow::bail!("fake channel {} already running", self.name);
            };
            loop {
                tokio::select! {
                    () = cancel.cancelled() => return Ok(()),
                    message = injected.recv() => match message {
                        Some(message) => {
                            if inbound.send(message).await.is_err() {
                                return Ok(());
                            }
                        }
                        None => return Ok(()),
                    },
                }
            }
        })
    }

    fn send<'a>(&'a self, message: Outbound) -> ChannelFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            self.sent.lock().unwrap().push(message);
            self.delivered.notify_waiters();
            Ok(())
        })
    }
}
