//! The loop: channels in, turns, channels out.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ilar::config::Config;
use ilar::provider::ProviderResolver;
use ilar::session::ImageContent;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::bus::{Inbound, Outbound, split_key};
use crate::channel::Channel;
use crate::config::{GatewayConfig, gateway_dir};
use crate::driver::{Driver, FollowUp, TurnError, log};
use crate::inbox::{self, RateLimit};
use crate::routes::RouteStore;

pub struct Gateway {
    driver: Arc<Driver>,
    routes: Arc<RouteStore>,
    channels: HashMap<String, Arc<dyn Channel>>,
    inbox_dir: PathBuf,
    rate: Mutex<RateLimit>,
    follow_ups: Mutex<Option<mpsc::Receiver<FollowUp>>>,
    cancel: CancellationToken,
}

/// What a chat is told when its session is held by another process.
pub const BUSY_REPLY: &str =
    "This chat's session is open somewhere else (a TUI, most likely); try again when it is closed.";

impl Gateway {
    pub fn new(
        config: Config,
        gateway: GatewayConfig,
        resolver: Arc<dyn ProviderResolver>,
        channels: Vec<Arc<dyn Channel>>,
    ) -> Result<Arc<Self>> {
        let dir = gateway_dir(&config);
        let routes = Arc::new(RouteStore::open(dir.join("routes.json"))?);
        let (follow_tx, follow_rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let rate = RateLimit::new(Duration::from_secs(gateway.notify_interval_secs));
        let driver = Arc::new(Driver::new(
            config,
            gateway,
            resolver,
            routes.clone(),
            follow_tx,
            cancel.clone(),
        ));
        Ok(Arc::new(Self {
            driver,
            routes,
            channels: channels
                .into_iter()
                .map(|channel| (channel.name().to_string(), channel))
                .collect(),
            inbox_dir: dir.join("inbox"),
            rate: Mutex::new(rate),
            follow_ups: Mutex::new(Some(follow_rx)),
            cancel,
        }))
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    pub fn inbox_dir(&self) -> &std::path::Path {
        &self.inbox_dir
    }

    /// Run until cancelled. Channels run on their own tasks; every
    /// message and follow-up is handled on its own task, and a chat's
    /// turns serialize on its seat.
    pub async fn run(self: Arc<Self>) -> Result<()> {
        let (inbound_tx, mut inbound) = mpsc::channel::<Inbound>(256);
        let mut channel_tasks = tokio::task::JoinSet::new();
        for channel in self.channels.values() {
            let channel = channel.clone();
            let tx = inbound_tx.clone();
            let cancel = self.cancel.child_token();
            channel_tasks.spawn(async move {
                let name = channel.name().to_string();
                if let Err(error) = channel.run(tx, cancel).await {
                    log(&format!("channel {name} stopped: {error:#}"));
                }
            });
        }
        let mut follow_ups = self
            .follow_ups
            .lock()
            .unwrap()
            .take()
            .context("gateway already running")?;
        let mut inbox_tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                () = self.cancel.cancelled() => break,
                message = inbound.recv() => match message {
                    Some(message) => {
                        let gateway = self.clone();
                        tokio::spawn(async move { gateway.handle_inbound(message).await });
                    }
                    None => break,
                },
                follow_up = follow_ups.recv() => match follow_up {
                    Some(follow_up) => {
                        let gateway = self.clone();
                        tokio::spawn(async move { gateway.handle_follow_up(follow_up).await });
                    }
                    None => break,
                },
                _ = inbox_tick.tick() => self.poll_inbox(&inbound_tx).await,
            }
        }
        channel_tasks.shutdown().await;
        self.driver.shutdown().await;
        Ok(())
    }

    async fn handle_inbound(&self, message: Inbound) {
        let key = message.session_key();
        let seat = match self.driver.seat(&key, &message.channel, &message.chat_id) {
            Ok(seat) => seat,
            Err(error) => {
                log(&format!("{key}: cannot open a session: {error:#}"));
                self.deliver(
                    &message.channel,
                    &message.chat_id,
                    &format!("error: {error:#}"),
                )
                .await;
                return;
            }
        };
        if let Err(error) = self
            .routes
            .update(|routes| routes.touch(&key, message.is_group))
        {
            log(&format!("{key}: routes not saved: {error:#}"));
        }
        let images = attachments(&message.media);
        log(&format!("{key}: turn ({} chars)", message.text.len()));
        match self.driver.run(&seat, &message.text, &images).await {
            Ok(report) => {
                self.deliver(&message.channel, &message.chat_id, &report.text)
                    .await;
            }
            Err(TurnError::Busy(why)) => {
                log(&format!("{key}: {why}"));
                self.deliver(&message.channel, &message.chat_id, BUSY_REPLY)
                    .await;
            }
            Err(TurnError::Failed(error)) => {
                log(&format!("{key}: turn failed: {error:#}"));
                self.deliver(
                    &message.channel,
                    &message.chat_id,
                    &format!("error: {error:#}"),
                )
                .await;
            }
        }
    }

    /// A child's completion, delivered to the root as a prompt — the
    /// same words a TUI would append — and retired from the outbox
    /// once the log holds them.
    async fn handle_follow_up(&self, follow_up: FollowUp) {
        let key = follow_up.session_key;
        let Some(seat) = self.driver.seats().into_iter().find(|seat| seat.key == key) else {
            log(&format!("{key}: follow-up for a chat with no seat"));
            return;
        };
        let notification = follow_up.notification;
        match self.driver.run(&seat, &notification.text, &[]).await {
            Ok(report) => {
                ilar::outbox::retire(&self.driver.outbox_dir(), &notification);
                self.deliver(&seat.channel, &seat.chat_id, &report.text)
                    .await;
            }
            Err(error) => log(&format!("{key}: follow-up not delivered: {error}")),
        }
    }

    async fn deliver(&self, channel: &str, chat_id: &str, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        let Some(target) = self.channels.get(channel) else {
            log(&format!("no channel named {channel}; dropping a reply"));
            return;
        };
        let message = Outbound {
            channel: channel.to_string(),
            chat_id: chat_id.to_string(),
            text: text.to_string(),
            media: Vec::new(),
        };
        if let Err(error) = target.send(message).await {
            log(&format!("{channel}:{chat_id}: send failed: {error:#}"));
        }
    }

    /// Messages left by `ilar-gateway notify`: rate-limited per source,
    /// addressed explicitly or to the last active chat.
    async fn poll_inbox(&self, inbound: &mpsc::Sender<Inbound>) {
        let waiting = match inbox::drain(&self.inbox_dir) {
            Ok(waiting) => waiting,
            Err(error) => {
                log(&format!("inbox unreadable: {error:#}"));
                return;
            }
        };
        for (path, message) in waiting {
            let _ = std::fs::remove_file(&path);
            if !self
                .rate
                .lock()
                .unwrap()
                .admit(&message.source, Instant::now())
            {
                log(&format!("inbox: {} rate-limited, dropped", message.source));
                continue;
            }
            let target = message
                .to
                .clone()
                .or_else(|| self.routes.snapshot().last_active);
            let Some((channel, chat_id)) = target.as_deref().and_then(split_key) else {
                log(&format!(
                    "inbox: no chat to deliver {} to; dropped",
                    message.source
                ));
                continue;
            };
            let _ = inbound
                .send(Inbound {
                    channel: channel.to_string(),
                    chat_id: chat_id.to_string(),
                    sender_id: format!("notify:{}", message.source),
                    text: message.text,
                    media: Vec::new(),
                    is_group: self
                        .routes
                        .snapshot()
                        .is_group(target.as_deref().unwrap_or_default()),
                })
                .await;
        }
    }
}

/// Image attachments the model can see; anything else is left to the
/// channel to mention in the text.
fn attachments(media: &[PathBuf]) -> Vec<ImageContent> {
    media
        .iter()
        .filter_map(|path| std::fs::read(path).ok())
        .filter_map(|bytes| ilar::image::from_file_bytes(&bytes))
        .collect()
}
