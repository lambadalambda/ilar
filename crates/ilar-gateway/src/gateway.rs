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

use crate::bus::{Inbound, Outbound, session_key, split_key};
use crate::channel::Channel;
use crate::config::{GatewayConfig, gateway_dir};
use crate::cron::CronStore;
use crate::driver::{Driver, FollowUp, TurnError, TurnReport, Wiring, log};
use crate::inbox::{self, RateLimit};
use crate::routes::RouteStore;

pub struct Gateway {
    driver: Arc<Driver>,
    routes: Arc<RouteStore>,
    channels: HashMap<String, Arc<dyn Channel>>,
    inbox_dir: PathBuf,
    rate: Mutex<RateLimit>,
    follow_ups: Mutex<Option<mpsc::Receiver<FollowUp>>>,
    /// One queue out, drained in order by one task: what the model
    /// sends and what the gateway says on its behalf leave in the
    /// order they were said.
    outbound_tx: mpsc::Sender<Outbound>,
    outbound: Mutex<Option<mpsc::Receiver<Outbound>>>,
    cron: Arc<CronStore>,
    settings: GatewayConfig,
    cancel: CancellationToken,
}

/// What a chat is told when its session is held by another process.
pub const BUSY_REPLY: &str =
    "This chat's session is open somewhere else (a TUI, most likely); try again when it is closed.";
/// The same, for a subagent's report that has to wait.
pub const BUSY_FOLLOW_UP: &str = "A subagent finished, but this chat's session is open somewhere else; its report is delivered once that closes.";
/// What a chat is told when a turn failed. The cause goes to the log:
/// an error chain names paths and provider bodies, and the chat may
/// not be the operator.
pub const FAILED_REPLY: &str = "That turn failed; the gateway log has the cause.";

/// How long a stop waits for turns in flight before giving up on them.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(15);

impl Gateway {
    pub fn new(
        config: Config,
        gateway: GatewayConfig,
        resolver: Arc<dyn ProviderResolver>,
        channels: Vec<Arc<dyn Channel>>,
    ) -> Result<Arc<Self>> {
        let dir = gateway_dir(&config);
        let routes = Arc::new(RouteStore::open(dir.join("routes.json"))?);
        let cron = Arc::new(CronStore::open(dir.join("cron.json"))?);
        let settings = gateway.clone();
        let (follow_tx, follow_rx) = mpsc::channel(64);
        let (outbound_tx, outbound_rx) = mpsc::channel(256);
        let cancel = CancellationToken::new();
        let rate = RateLimit::new(Duration::from_secs(gateway.notify_interval_secs));
        let constraints = channels
            .iter()
            .map(|channel| {
                (
                    channel.name().to_string(),
                    channel.constraints().to_string(),
                )
            })
            .collect();
        let driver = Arc::new(Driver::new(
            config,
            gateway,
            resolver,
            routes.clone(),
            Wiring {
                follow_ups: follow_tx,
                outbound: outbound_tx.clone(),
                constraints,
                cron: cron.clone(),
            },
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
            outbound_tx,
            outbound: Mutex::new(Some(outbound_rx)),
            cron,
            settings,
            cancel,
        }))
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    pub fn inbox_dir(&self) -> &std::path::Path {
        &self.inbox_dir
    }

    /// The tools a chat's model can see, once it has a seat.
    pub fn tool_names(&self, key: &str) -> Option<Vec<&'static str>> {
        self.driver
            .seat_by_key(key)
            .map(|seat| seat.runtime.registry.tool_names())
    }

    /// Each agent a chat may spawn, with the tools it is restricted to
    /// (`None` is unrestricted).
    pub fn agent_tools(&self, key: &str) -> Option<Vec<(String, Option<Vec<String>>)>> {
        self.driver.seat_by_key(key).map(|seat| {
            seat.runtime
                .spawner
                .agents()
                .iter()
                .map(|agent| (agent.name.clone(), agent.tools.clone()))
                .collect()
        })
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
        let mut outbound = self
            .outbound
            .lock()
            .unwrap()
            .take()
            .context("gateway already running")?;
        let dispatcher = {
            let gateway = self.clone();
            tokio::spawn(async move {
                while let Some(message) = outbound.recv().await {
                    gateway.send(message).await;
                }
            })
        };
        let mut inbox_tick = tokio::time::interval(Duration::from_secs(1));
        let mut scheduler_tick = tokio::time::interval(Duration::from_secs(
            self.settings.scheduler_tick_secs.max(1),
        ));
        let mut last_heartbeat: HashMap<String, Instant> = HashMap::new();
        let mut handlers = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                () = self.cancel.cancelled() => break,
                message = inbound.recv() => match message {
                    Some(message) => {
                        let gateway = self.clone();
                        handlers.spawn(async move { gateway.handle_inbound(message).await });
                    }
                    None => break,
                },
                follow_up = follow_ups.recv() => match follow_up {
                    Some(follow_up) => {
                        let gateway = self.clone();
                        handlers.spawn(async move { gateway.handle_follow_up(follow_up).await });
                    }
                    None => break,
                },
                Some(_) = handlers.join_next(), if !handlers.is_empty() => {}
                _ = inbox_tick.tick() => self.poll_inbox(&inbound_tx).await,
                _ = scheduler_tick.tick() => {
                    for (key, target, prompt) in self.due_now(&mut last_heartbeat) {
                        let gateway = self.clone();
                        handlers.spawn(async move { gateway.handle_scheduled(key, target, prompt).await });
                    }
                }
            }
        }
        // Turns in flight see the cancellation and wind down; give them
        // the time to, then stop waiting.
        channel_tasks.shutdown().await;
        let drain = async { while handlers.join_next().await.is_some() {} };
        if tokio::time::timeout(SHUTDOWN_GRACE, drain).await.is_err() {
            log("stopping with turns still in flight");
            handlers.abort_all();
        }
        self.driver.shutdown().await;
        // The senders are gone with the seats; the dispatcher ends when
        // the queue is empty, so nothing said during the grace is lost.
        let _ = tokio::time::timeout(SHUTDOWN_GRACE, dispatcher).await;
        Ok(())
    }

    async fn handle_inbound(&self, message: Inbound) {
        let key = message.session_key();
        let seat = match self
            .driver
            .seat(&key, &message.channel, &message.chat_id, message.is_group)
            .await
        {
            Ok(seat) => seat,
            Err(error) => {
                log(&format!("{key}: cannot open a session: {error:#}"));
                self.deliver(&message.channel, &message.chat_id, FAILED_REPLY)
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
        log(&format!(
            "{key}: turn from {} ({} chars)",
            message.sender_id,
            message.text.len()
        ));
        match self.driver.run(&seat, &message.text, &images).await {
            Ok(report) => self.deliver_unless_sent(&seat, &report).await,
            Err(TurnError::Busy(why)) => {
                log(&format!("{key}: {why}"));
                self.deliver(&message.channel, &message.chat_id, BUSY_REPLY)
                    .await;
            }
            Err(TurnError::Failed(error)) => {
                log(&format!("{key}: turn failed: {error:#}"));
                self.deliver(&message.channel, &message.chat_id, FAILED_REPLY)
                    .await;
            }
        }
    }

    /// A child's completion, delivered to the root as a prompt — the
    /// same words a TUI would append — and retired from the outbox
    /// once the log holds them. A held writer means wait and try
    /// again; a failed turn leaves the entry for the next start.
    async fn handle_follow_up(&self, follow_up: FollowUp) {
        let key = follow_up.session_key.clone();
        let Some(seat) = self.driver.seat_by_key(&key) else {
            log(&format!("{key}: follow-up for a chat with no seat"));
            return;
        };
        match self.driver.run(&seat, &follow_up.prompt, &[]).await {
            Ok(report) => {
                ilar::outbox::retire(&self.driver.outbox_dir(), &follow_up.retire);
                self.deliver_unless_sent(&seat, &report).await;
            }
            Err(TurnError::Busy(why)) => {
                log(&format!("{key}: follow-up waits: {why}"));
                self.deliver(&seat.channel, &seat.chat_id, BUSY_FOLLOW_UP)
                    .await;
                self.driver.requeue(follow_up);
            }
            Err(TurnError::Failed(error)) => {
                log(&format!("{key}: follow-up failed: {error:#}"));
                self.deliver(&seat.channel, &seat.chat_id, FAILED_REPLY)
                    .await;
            }
        }
    }

    /// Everything whose time has come: due cron jobs, and a heartbeat
    /// for every configured chat whose interval has passed. Each is a
    /// session key, the chat it speaks to, and the prompt.
    fn due_now(
        &self,
        last_heartbeat: &mut HashMap<String, Instant>,
    ) -> Vec<(String, String, String)> {
        let mut due = Vec::new();
        match self.cron.take_due(chrono::Utc::now()) {
            Ok(jobs) => {
                for job in jobs {
                    due.push((job.session_key(), job.target.clone(), job.prompt.clone()));
                }
            }
            Err(error) => log(&format!("cron: {error:#}")),
        }
        let heartbeat = &self.settings.heartbeat;
        if heartbeat.every_secs > 0 {
            let interval = Duration::from_secs(heartbeat.every_secs);
            let now = Instant::now();
            for chat in &heartbeat.chats {
                let beat = last_heartbeat
                    .get(chat)
                    .is_none_or(|last| now.duration_since(*last) >= interval);
                if beat {
                    last_heartbeat.insert(chat.clone(), now);
                    due.push((
                        format!("heartbeat:{chat}"),
                        chat.clone(),
                        heartbeat.prompt.clone(),
                    ));
                }
            }
        }
        due
    }

    /// A cron or heartbeat turn: its own session, homed on the chat it
    /// is for, and heard from only through the message tool.
    async fn handle_scheduled(&self, key: String, target: String, prompt: String) {
        let Some((channel, chat_id)) = split_key(&target) else {
            log(&format!("{key}: target {target:?} is not channel:chat"));
            return;
        };
        if self.routes.snapshot().session_for(&target).is_none() {
            log(&format!(
                "{key}: target {target} has never written; skipped"
            ));
            return;
        }
        let seat = match self.driver.background_seat(&key, channel, chat_id).await {
            Ok(seat) => seat,
            Err(error) => {
                log(&format!("{key}: cannot open a session: {error:#}"));
                return;
            }
        };
        log(&format!("{key}: scheduled turn for {target}"));
        match self.driver.run(&seat, &prompt, &[]).await {
            Ok(report) if report.sent == 0 => log(&format!("{key}: nothing to say")),
            Ok(report) => log(&format!("{key}: {} message(s) sent", report.sent)),
            Err(error) => log(&format!("{key}: scheduled turn failed: {error}")),
        }
    }

    /// The final text of a turn goes out only when the model sent
    /// nothing itself; a model that used the message tool has said
    /// what it wanted to say. A background seat has no final text to
    /// deliver at all.
    async fn deliver_unless_sent(&self, seat: &crate::driver::Seat, report: &TurnReport) {
        if report.sent > 0 {
            log(&format!(
                "{}: {} message(s) sent by the model",
                seat.key, report.sent
            ));
            return;
        }
        if seat.background {
            return;
        }
        self.deliver(&seat.channel, &seat.chat_id, &report.text)
            .await;
    }

    /// One outbound message, to its channel. Only the dispatcher calls
    /// this, one message at a time.
    async fn send(&self, message: Outbound) {
        let key = session_key(&message.channel, &message.chat_id);
        let Some(target) = self.channels.get(&message.channel) else {
            log(&format!("{key}: no such channel; dropping a message"));
            return;
        };
        if let Err(error) = target.send(message).await {
            log(&format!("{key}: send failed: {error:#}"));
        }
    }

    /// Something the gateway says on the model's behalf, through the
    /// same queue as the model's own sends, so it never overtakes them.
    async fn deliver(&self, channel: &str, chat_id: &str, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        let message = Outbound {
            channel: channel.to_string(),
            chat_id: chat_id.to_string(),
            text: text.to_string(),
            media: Vec::new(),
        };
        if self.outbound_tx.send(message).await.is_err() {
            log(&format!(
                "{channel}:{chat_id}: the dispatcher is gone; dropping a reply"
            ));
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
