//! A channel: somewhere a person types, and somewhere the answer goes.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::bus::{Inbound, Outbound};

pub type ChannelFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Whether a file is a picture to show as one, by its extension — the
/// one rule every channel applies before choosing how to send a file.
pub fn is_image(path: &std::path::Path) -> bool {
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp")
    )
}

pub trait Channel: Send + Sync {
    fn name(&self) -> &str;

    /// What the model is told about sending here: formatting, length,
    /// how files travel. Empty when there is nothing to say.
    fn constraints(&self) -> &str {
        ""
    }

    /// Run until cancelled, publishing everything received.
    fn run<'a>(
        &'a self,
        inbound: mpsc::Sender<Inbound>,
        cancel: CancellationToken,
    ) -> ChannelFuture<'a, anyhow::Result<()>>;

    fn send<'a>(&'a self, message: Outbound) -> ChannelFuture<'a, anyhow::Result<()>>;

    /// Post a status line the chat can watch while a turn runs, and
    /// hand back what edits and clears it later. `None`: this channel
    /// has no such thing.
    fn post_status<'a>(
        &'a self,
        _chat_id: &'a str,
        _text: &'a str,
    ) -> ChannelFuture<'a, anyhow::Result<Option<String>>> {
        Box::pin(async { Ok(None) })
    }

    fn edit_status<'a>(
        &'a self,
        _chat_id: &'a str,
        _status_id: &'a str,
        _text: &'a str,
    ) -> ChannelFuture<'a, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn clear_status<'a>(
        &'a self,
        _chat_id: &'a str,
        _status_id: &'a str,
    ) -> ChannelFuture<'a, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Take a message back out of the chat — a `/unlock` with the
    /// master password in it. `false`: this channel cannot, so the
    /// person is the one who has to delete it.
    fn delete_message<'a>(
        &'a self,
        _chat_id: &'a str,
        _message_id: &'a str,
    ) -> ChannelFuture<'a, anyhow::Result<bool>> {
        Box::pin(async { Ok(false) })
    }
}

/// What a fake channel saw of a status line, in order with the sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seen {
    Sent(String),
    StatusPosted(String),
    StatusEdited(String),
    StatusCleared,
    /// A message taken back out of the chat, by its id.
    Deleted(String),
}

/// A channel with nobody behind it: tests inject what a person would
/// have typed and read what they would have seen.
pub struct FakeChannel {
    name: String,
    injector: mpsc::Sender<Inbound>,
    injected: Mutex<Option<mpsc::Receiver<Inbound>>>,
    sent: Mutex<Vec<Outbound>>,
    /// Everything, sends and status changes, in the order it happened.
    seen: Mutex<Vec<Seen>>,
    delivered: tokio::sync::Notify,
    /// Runs left that fail right away, for a test of the restart.
    failing_runs: std::sync::atomic::AtomicUsize,
    runs: std::sync::atomic::AtomicUsize,
    /// Injected messages, for the ids they carry.
    injections: std::sync::atomic::AtomicUsize,
    /// Sends left that the channel refuses, for a test of the retry.
    failing_sends: std::sync::atomic::AtomicUsize,
}

impl FakeChannel {
    pub fn new(name: &str) -> Arc<Self> {
        let (injector, injected) = mpsc::channel(64);
        Arc::new(Self {
            name: name.to_string(),
            injector,
            injected: Mutex::new(Some(injected)),
            sent: Mutex::new(Vec::new()),
            seen: Mutex::new(Vec::new()),
            delivered: tokio::sync::Notify::new(),
            failing_runs: std::sync::atomic::AtomicUsize::new(0),
            runs: std::sync::atomic::AtomicUsize::new(0),
            injections: std::sync::atomic::AtomicUsize::new(0),
            failing_sends: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    /// Make the next `count` runs fail at once, as a dead server would.
    pub fn fail_next_runs(&self, count: usize) {
        self.failing_runs
            .store(count, std::sync::atomic::Ordering::Release);
    }

    /// Refuse the next `count` sends, as a channel that is down does.
    pub fn fail_next_sends(&self, count: usize) {
        self.failing_sends
            .store(count, std::sync::atomic::Ordering::Release);
    }

    pub fn runs(&self) -> usize {
        self.runs.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Refusals still to come, for a test that has to know the first
    /// send was attempted before it does anything else.
    pub fn refusals_left(&self) -> usize {
        self.failing_sends
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// What a person typed. Buffered, so it may precede `run`.
    pub async fn inject(&self, text: &str, chat_id: &str, sender_id: &str) {
        self.inject_with(text, chat_id, sender_id, false).await;
    }

    /// The same, in a group.
    pub async fn inject_in_group(&self, text: &str, chat_id: &str, sender_id: &str) {
        self.inject_with(text, chat_id, sender_id, true).await;
    }

    /// With attachments, as files the channel fetched.
    pub async fn inject_with_media(
        &self,
        text: &str,
        chat_id: &str,
        media: Vec<std::path::PathBuf>,
    ) {
        self.injector
            .send(Inbound {
                channel: self.name.clone(),
                chat_id: chat_id.to_string(),
                sender_id: "alice".into(),
                message_id: Some(self.next_message_id()),
                text: text.to_string(),
                media,
                is_group: false,
            })
            .await
            .expect("fake channel receiver dropped");
    }

    async fn inject_with(&self, text: &str, chat_id: &str, sender_id: &str, is_group: bool) {
        self.injector
            .send(Inbound {
                channel: self.name.clone(),
                chat_id: chat_id.to_string(),
                sender_id: sender_id.to_string(),
                message_id: Some(self.next_message_id()),
                text: text.to_string(),
                media: Vec::new(),
                is_group,
            })
            .await
            .expect("fake channel receiver dropped");
    }

    /// The id the next injected message carries, as a channel's own
    /// ids are: unique, and the gateway's only handle on it.
    fn next_message_id(&self) -> String {
        let n = self
            .injections
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        format!("msg-{n}")
    }

    pub fn sent(&self) -> Vec<Outbound> {
        self.sent.lock().unwrap().clone()
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    /// Wait until at least `count` messages went out, or the timeout.
    pub async fn wait_for_sent(&self, count: usize, timeout: std::time::Duration) -> Vec<Outbound> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            // Armed before the check, so a send between the two is
            // not a wakeup missed.
            let notified = self.delivered.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let sent = self.sent();
            if sent.len() >= count {
                return sent;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
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
            self.runs.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            let failing = self.failing_runs.load(std::sync::atomic::Ordering::Acquire);
            if failing > 0 {
                self.failing_runs
                    .store(failing - 1, std::sync::atomic::Ordering::Release);
                anyhow::bail!("fake channel {}: the server died", self.name);
            }
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
            let refusing = self
                .failing_sends
                .load(std::sync::atomic::Ordering::Acquire);
            if refusing > 0 {
                self.failing_sends
                    .store(refusing - 1, std::sync::atomic::Ordering::Release);
                anyhow::bail!("fake channel {}: the wire is down", self.name);
            }
            self.seen
                .lock()
                .unwrap()
                .push(Seen::Sent(message.text.clone()));
            self.sent.lock().unwrap().push(message);
            self.delivered.notify_waiters();
            Ok(())
        })
    }

    fn post_status<'a>(
        &'a self,
        _chat_id: &'a str,
        text: &'a str,
    ) -> ChannelFuture<'a, anyhow::Result<Option<String>>> {
        Box::pin(async move {
            self.seen
                .lock()
                .unwrap()
                .push(Seen::StatusPosted(text.to_string()));
            Ok(Some("status-1".to_string()))
        })
    }

    fn edit_status<'a>(
        &'a self,
        _chat_id: &'a str,
        _status_id: &'a str,
        text: &'a str,
    ) -> ChannelFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            self.seen
                .lock()
                .unwrap()
                .push(Seen::StatusEdited(text.to_string()));
            Ok(())
        })
    }

    fn clear_status<'a>(
        &'a self,
        _chat_id: &'a str,
        _status_id: &'a str,
    ) -> ChannelFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            self.seen.lock().unwrap().push(Seen::StatusCleared);
            Ok(())
        })
    }

    fn delete_message<'a>(
        &'a self,
        _chat_id: &'a str,
        message_id: &'a str,
    ) -> ChannelFuture<'a, anyhow::Result<bool>> {
        Box::pin(async move {
            self.seen
                .lock()
                .unwrap()
                .push(Seen::Deleted(message_id.to_string()));
            Ok(true)
        })
    }
}
