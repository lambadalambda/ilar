//! The loop: channels in, turns, channels out.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ilar::config::Config;
use ilar::memory::MemoryStore;
use ilar::provider::ProviderResolver;
use ilar::session::ImageContent;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::bus::{Inbound, Outbound, session_key, split_key};
use crate::channel::Channel;
use crate::commands::{self, Command};
use crate::config::GatewayConfig;
use crate::cron::CronStore;
use crate::driver::{Driver, FollowUp, ModelSwitch, TurnError, TurnReport, Wiring, log};
use crate::inbox::{self, RateLimit};
use crate::routes::RouteStore;

pub struct Gateway {
    driver: Arc<Driver>,
    routes: Arc<RouteStore>,
    channels: HashMap<String, Arc<dyn Channel>>,
    inbox_dir: PathBuf,
    rate: Mutex<RateLimit>,
    follow_ups: Mutex<Option<mpsc::Receiver<FollowUp>>>,
    /// One queue out, fanned into a lane per channel by the dispatcher:
    /// what the model sends and what the gateway says on its behalf
    /// leave each channel in the order they were said, and no channel
    /// waits on another's retries.
    outbound_tx: mpsc::Sender<Outbound>,
    outbound: Mutex<Option<mpsc::Receiver<Outbound>>>,
    /// Raised at the end of a shutdown: the dispatcher drains what is
    /// queued and ends, instead of waiting on senders the seats keep.
    outbound_closed: CancellationToken,
    cron: Arc<CronStore>,
    memory: Arc<MemoryStore>,
    skills: Arc<crate::skills::SkillLibrary>,
    pending: crate::review::PendingStore,
    settings: GatewayConfig,
    /// The gateway's own handle, for tasks it spawns on itself.
    me: std::sync::Weak<Gateway>,
    /// The status line each chat is watching, while a turn runs there.
    status: Arc<crate::status::StatusBoard>,
    cancel: CancellationToken,
    /// `/restart` was asked for: the stop under way should end the
    /// process with [`RESTART_EXIT`], which the service unit restarts
    /// on, rather than cleanly.
    restart: std::sync::atomic::AtomicBool,
}

/// The exit code `/restart` ends the process with. Not zero, so a
/// service unit with `Restart=on-failure` starts the gateway again;
/// `EX_TEMPFAIL` in sysexits, which is what it means.
pub const RESTART_EXIT: i32 = 75;

/// Something whose time has come: a cron job, or a heartbeat.
struct Due {
    /// The session it runs on: `cron:<id>` or `heartbeat:<key>`.
    key: String,
    /// What it is called when the chat has to be told it failed.
    name: String,
    /// The chat it speaks to, or `LAST_ACTIVE`.
    target: String,
    prompt: String,
    /// The job it came from. `None` for a heartbeat, which is silent
    /// by design — including about its own failures.
    job: Option<crate::cron::Job>,
}

/// What a chat is told when its turn was cancelled.
pub const ABORTED_REPLY: &str = "Aborted.";

/// The same, when nobody asked for it: the gateway is going down and
/// every turn in flight goes with it. The prompt is not re-run — the
/// person is the one who knows whether it still matters.
pub const RESTARTING_REPLY: &str = "Aborted: the gateway is restarting; send that again.";

/// Which of the two a cancelled turn is told.
///
/// What the person did beats what the process is doing. This read only
/// `shutting_down`, at delivery time, so an `/abort` that landed a
/// second before a restart was answered "the gateway is restarting;
/// send that again" — the opposite of what was asked for, and an
/// invitation to re-run work the person had just stopped.
pub fn aborted_reply(asked_for: bool, shutting_down: bool) -> &'static str {
    if asked_for || !shutting_down {
        ABORTED_REPLY
    } else {
        RESTARTING_REPLY
    }
}

/// The same for a subagent's report, which nobody has to send again:
/// its outbox entry stays and the next start delivers it.
pub const RESTARTING_FOLLOW_UP: &str =
    "The gateway is restarting; a subagent's report is delivered once it is back.";

/// What a chat is told when the model ended its turn with no message
/// and no text.
pub const EMPTY_REPLY: &str =
    "The model ended its turn without a reply. Send that again, or switch with /model.";

/// What a chat is told when its message was folded into the turn
/// running now and there is no status line to show that on.
pub const STEER_ACK: &str =
    "Got that — it goes into the turn running now, and one reply covers both.";

/// What a chat is told when its session is held by another process.
pub const BUSY_REPLY: &str =
    "This chat's session is open somewhere else (a TUI, most likely); try again when it is closed.";
/// The same, for a subagent's report that has to wait.
pub const BUSY_FOLLOW_UP: &str = "A subagent finished, but this chat's session is open somewhere else; its report is delivered once that closes.";
/// What a chat is told when something failed: what, and the cause,
/// clipped. The chat is allowlisted to its operator, who wants the
/// reason where they are; the full chain is in the log regardless.
pub fn failed_reply(what: &str, error: &anyhow::Error) -> String {
    failed_line(what, &format!("{error:#}"))
}

/// The same for a cause that is already a line of its own.
pub fn failed_line(what: &str, cause: &str) -> String {
    let cause: String = cause.split_whitespace().collect::<Vec<_>>().join(" ");
    let cause = if cause.chars().count() > FAILURE_CAUSE_CHARS {
        let mut cut: String = cause.chars().take(FAILURE_CAUSE_CHARS - 1).collect();
        cut.push('…');
        cut
    } else {
        cause
    };
    format!("{what} failed: {cause}")
}

/// How much of a failure's cause goes into the chat.
const FAILURE_CAUSE_CHARS: usize = 400;

/// What to do about a password now sitting in the chat: a `/unlock` or
/// a `/password` is in the history of every device it synced to, and
/// the channel may or may not be able to take it back.
pub fn password_advice(taken_back: bool) -> &'static str {
    if taken_back {
        "I deleted that message; check that the password is gone on your other devices too."
    } else {
        "Delete that message: the password stays in this chat's history otherwise."
    }
}

/// The same, for a command name that only *looked* like `/unlock`. The
/// reach that catches `/unlok` also catches `/lock` and `/block`, and
/// telling someone who typed one of those to go audit their devices
/// for a leaked password is alarming and untrue. The deletion still
/// happens — the guess is about the word, not about the risk.
pub fn maybe_password_advice(taken_back: bool) -> &'static str {
    if taken_back {
        "I deleted that message, in case there was a password in it."
    } else {
        "Delete that message if there was a password in it."
    }
}

/// How long a stop waits for turns in flight before giving up on them.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(15);
/// The pause before a channel that stopped is started again.
const CHANNEL_RESTART: Duration = Duration::from_secs(5);
/// How many times a refused send is tried; the pause between them is
/// `gateway.send_retry_secs`.
const SEND_TRIES: u32 = 4;

/// The dispatcher's lanes: one sender task per channel, fed in the
/// order messages were queued, so a channel keeps its order and no
/// channel waits on another's retries. Unbounded on purpose — the
/// dispatcher must never block on a lane, or a dead channel would hold
/// the others through the queue instead of through the send.
#[derive(Default)]
struct Lanes {
    senders: HashMap<String, mpsc::UnboundedSender<Outbound>>,
    workers: tokio::task::JoinSet<()>,
}

impl Lanes {
    fn dispatch(&mut self, gateway: &Arc<Gateway>, message: Outbound) {
        let lane = self
            .senders
            .entry(message.channel.clone())
            .or_insert_with(|| {
                let (tx, mut rx) = mpsc::unbounded_channel::<Outbound>();
                let gateway = gateway.clone();
                self.workers.spawn(async move {
                    while let Some(message) = rx.recv().await {
                        gateway.send(message).await;
                    }
                });
                tx
            });
        // A lane's worker ends only when its sender is dropped, which
        // `finish` does — or when `send` panicked inside it. That lane
        // would otherwise swallow every later message for the channel,
        // silently, for the life of the process: say so, and let the
        // next message start a fresh worker.
        if let Err(mpsc::error::SendError(message)) = lane.send(message) {
            log(&format!(
                "{}: its sender lane died; dropping a message",
                message.channel
            ));
            self.senders.remove(&message.channel);
        }
    }

    /// Close every lane and wait for what they hold to go out.
    async fn finish(mut self) {
        self.senders.clear();
        while self.workers.join_next().await.is_some() {}
    }
}
/// How long after a failed one-shot job it is tried once more.
const ONE_SHOT_RETRY: chrono::TimeDelta = chrono::TimeDelta::minutes(1);
/// How long the start announcement keeps trying while the channel
/// connects, and how long the stop announcement may take.
const ANNOUNCE_RETRY: Duration = Duration::from_secs(2);
const ANNOUNCE_TRIES: u32 = 30;
const ANNOUNCE_GRACE: Duration = Duration::from_secs(5);
const ANNOUNCE_SETTLE: Duration = Duration::from_millis(1500);

/// How long `/restart`'s reply gets to leave before the stop begins.
const RESTART_GRACE: Duration = Duration::from_secs(1);

/// `3m 20s`, `1h 5m`, `12s`: a duration as a chat reads one. Not the
/// core's `text::format_duration`, which has no hours and would print
/// a daily job as `1440m 0s`.
fn human_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    match (secs / 3600, (secs % 3600) / 60, secs % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m {s}s"),
        (h, m, _) => format!("{h}h {m}m"),
    }
}

/// `1,234,567`.
fn with_commas(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// What the start line says about this build.
pub fn build_line() -> String {
    let commit = env!("ILAR_GATEWAY_COMMIT");
    if commit.is_empty() {
        format!("ilar-gateway {}", env!("CARGO_PKG_VERSION"))
    } else {
        format!("ilar-gateway {} ({commit})", env!("CARGO_PKG_VERSION"))
    }
}
/// Several steers as one prompt: texts in order, attachments together.
fn fold_steers(steers: Vec<ilar::agent::Steer>) -> ilar::agent::Steer {
    let text = steers
        .iter()
        .map(|steer| steer.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let images: Vec<ImageContent> = steers.into_iter().flat_map(|steer| steer.images).collect();
    ilar::agent::Steer { text, images }
}

impl Gateway {
    pub fn new(
        config: Config,
        gateway: GatewayConfig,
        resolver: Arc<dyn ProviderResolver>,
        channels: Vec<Arc<dyn Channel>>,
    ) -> Result<Arc<Self>> {
        let dir = gateway.home(&config);
        let routes = Arc::new(RouteStore::open(dir.join("routes.json"))?);
        let cron = Arc::new(CronStore::open(dir.join("cron.json"))?);
        let memory = Arc::new(MemoryStore::new(dir.join("memory")));
        let skills = Arc::new(crate::skills::SkillLibrary::new(dir.join("skills")));
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
        let channels: HashMap<String, Arc<dyn Channel>> = channels
            .into_iter()
            .map(|channel| (channel.name().to_string(), channel))
            .collect();
        let status = crate::status::StatusBoard::new(
            channels.clone(),
            gateway.status,
            Duration::from_secs(gateway.status_interval_secs),
        );
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
                memory: memory.clone(),
                skills: skills.clone(),
                status: status.clone(),
            },
            cancel.clone(),
        ));
        Ok(Arc::new_cyclic(|me| Self {
            driver,
            routes,
            channels,
            inbox_dir: dir.join("inbox"),
            rate: Mutex::new(rate),
            follow_ups: Mutex::new(Some(follow_rx)),
            outbound_tx,
            outbound_closed: CancellationToken::new(),
            outbound: Mutex::new(Some(outbound_rx)),
            cron,
            memory,
            skills,
            pending: crate::review::PendingStore::new(dir.join("pending")),
            settings,
            status,
            me: me.clone(),
            cancel,
            restart: std::sync::atomic::AtomicBool::new(false),
        }))
    }

    /// Whether the stop under way was a `/restart`: the process should
    /// exit with [`RESTART_EXIT`].
    pub fn restart_requested(&self) -> bool {
        self.restart.load(std::sync::atomic::Ordering::Acquire)
    }

    /// An owning handle to this gateway, for a task it spawns.
    fn clone_handle(&self) -> Arc<Self> {
        self.me
            .upgrade()
            .expect("the gateway is alive while it runs")
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    pub fn inbox_dir(&self) -> &std::path::Path {
        &self.inbox_dir
    }

    /// A seat's system prompt, for a test that checks what a chat is
    /// told.
    pub fn system_prompt(&self, key: &str) -> Option<String> {
        self.driver
            .seat_by_key(key)
            .map(|seat| seat.runtime.system_prompt.clone())
    }

    /// What a chat on the first channel would be sent, as text: the
    /// system prompt with the situation and memory blocks, every tool
    /// with its schema. `private` false shows what a room gets.
    pub fn preview(&self, private: bool) -> Result<String> {
        let channel = self
            .channels
            .keys()
            .min()
            .cloned()
            .unwrap_or_else(|| "channel".to_string());
        Ok(self.driver.preview(&channel, "chat", private)?.render())
    }

    /// The tools a chat's model can see, once it has a seat.
    pub fn tool_names(&self, key: &str) -> Option<Vec<&'static str>> {
        self.driver
            .seat_by_key(key)
            .map(|seat| seat.runtime.registry.published_tool_names())
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
                // A channel that stops — its server died, its socket
                // dropped — is started again after a pause, for as
                // long as the gateway runs. Only a cancel ends it.
                let name = channel.name().to_string();
                loop {
                    match channel.run(tx.clone(), cancel.clone()).await {
                        Ok(()) => return,
                        Err(error) => log(&format!("channel {name} stopped: {error:#}")),
                    }
                    tokio::select! {
                        () = cancel.cancelled() => return,
                        () = tokio::time::sleep(CHANNEL_RESTART) => {
                            log(&format!("channel {name}: starting again"));
                        }
                    }
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
        // The weekly review is a job the gateway owns: present and
        // current while it is on, gone while it is off.
        let weekly = &self.settings.weekly;
        let outcome = if weekly.enabled {
            self.cron
                .upsert(
                    crate::cron::Job {
                        id: crate::weekly::JOB_ID.into(),
                        name: "weekly review".into(),
                        schedule: crate::cron::Schedule::Cron {
                            expr: weekly.cron.clone(),
                        },
                        prompt: crate::weekly::PROMPT.into(),
                        target: crate::cron::LAST_ACTIVE.into(),
                        next_run: None,
                        last_run: None,
                        retries: 0,
                    },
                    chrono::Utc::now(),
                )
                .map(drop)
        } else {
            self.cron.remove(crate::weekly::JOB_ID).map(drop)
        };
        if let Err(error) = outcome {
            log(&format!("weekly review not scheduled: {error:#}"));
        }
        let dispatcher = {
            let gateway = self.clone();
            let closed = self.outbound_closed.clone();
            tokio::spawn(async move {
                // One lane per channel, each sending in order. A channel
                // that is down retries on its own lane; the others carry
                // on. One queue for all of them meant one refused
                // message held every chat's replies for as long as its
                // retries took.
                let mut lanes = Lanes::default();
                loop {
                    tokio::select! {
                        biased;
                        message = outbound.recv() => match message {
                            Some(message) => lanes.dispatch(&gateway, message),
                            None => break,
                        },
                        () = closed.cancelled() => {
                            while let Ok(message) = outbound.try_recv() {
                                lanes.dispatch(&gateway, message);
                            }
                            break;
                        }
                    }
                }
                // Nothing queued is lost: every lane sends what it holds,
                // then ends.
                lanes.finish().await;
            })
        };
        let mut handlers = tokio::task::JoinSet::new();
        if self.settings.announce {
            let gateway = self.clone();
            handlers.spawn(async move { gateway.announce_start().await });
        }
        if self.driver.secret_store().is_locked() {
            log(
                "secret store is sealed and locked: /unlock <master password> from a chat opens it \
                 (a provider key kept in the store is unreadable until then)",
            );
        }
        let mut inbox_tick = tokio::time::interval(Duration::from_secs(1));
        let mut scheduler_tick = tokio::time::interval(Duration::from_secs(
            self.settings.scheduler_tick_secs.max(1),
        ));
        let mut last_heartbeat: HashMap<String, Instant> = HashMap::new();
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
                    for due in self.due_now(&mut last_heartbeat) {
                        let gateway = self.clone();
                        handlers.spawn(async move { gateway.handle_scheduled(due).await });
                    }
                }
            }
        }
        // Say goodbye while the channels are still up.
        if self.settings.announce {
            let _ = tokio::time::timeout(ANNOUNCE_GRACE, self.announce_stop()).await;
        }
        // Turns in flight see the cancellation and wind down; give them
        // the time to, then stop waiting.
        channel_tasks.shutdown().await;
        let drain = async { while handlers.join_next().await.is_some() {} };
        if tokio::time::timeout(SHUTDOWN_GRACE, drain).await.is_err() {
            log("stopping with turns still in flight");
            handlers.abort_all();
        }
        // A turn killed above never reached the `end` that takes its
        // own line down, so its "working…" bubble stayed in the chat —
        // still there at the next start, describing a turn that died
        // with the process. Done here rather than beside `announce_stop`
        // so it also covers the turns that wound down cleanly but hit
        // the grace.
        let _ = tokio::time::timeout(ANNOUNCE_GRACE, self.status.clear_all()).await;
        self.driver.shutdown().await;
        // Nothing said during the grace is lost: the dispatcher drains
        // the queue, then ends.
        self.outbound_closed.cancel();
        let _ = tokio::time::timeout(SHUTDOWN_GRACE, dispatcher).await;
        Ok(())
    }

    async fn handle_inbound(&self, message: Inbound) {
        let key = message.session_key();
        // Only a person types commands: a script reporting "/new" is
        // reporting, not asking for a fresh session.
        if !inbox::is_script(&message.sender_id)
            && let Some(command) = commands::parse(&message.text)
        {
            let reply = self.command(&key, &message, command).await;
            self.deliver(&message.channel, &message.chat_id, &reply)
                .await;
            return;
        }
        let seat = match self
            .driver
            .seat(&key, &message.channel, &message.chat_id, message.is_group)
            .await
        {
            Ok(seat) => seat,
            Err(error) => {
                log(&format!("{key}: cannot open a session: {error:#}"));
                self.deliver(
                    &message.channel,
                    &message.chat_id,
                    &failed_reply("Opening this chat's session", &error),
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
        let (images, notes) = attachments(&message.media);
        // In a room the model is told who is talking: several people
        // are, and "you" is not one of them. A private chat is one
        // person, and the name would be noise.
        let spoken = match (&message.sender_name, message.is_group) {
            (Some(name), true) => format!("{name}: {}", message.text),
            _ => message.text.clone(),
        };
        let prompt = format!("{spoken}{notes}");
        // A turn already running here reads the message at its next
        // step, as the TUI's steering does; its reply covers both.
        if self.driver.steer(&seat, &prompt, &images) {
            log(&format!(
                "{key}: steer from {} ({} chars)",
                message.sender_id,
                message.text.len()
            ));
            // With a status line up, the turn says "steered: …" there.
            // With none — the setting is off, or the line came down
            // with a reply already sent this turn — a folded-in
            // correction would look exactly like a dropped message.
            // A script is not waiting for an answer, so it gets none.
            if !inbox::is_script(&message.sender_id) && !self.status.is_up(&key) {
                self.deliver(&message.channel, &message.chat_id, STEER_ACK)
                    .await;
            }
            return;
        }
        log(&format!(
            "{key}: turn from {} ({} chars, {} attachment(s))",
            message.sender_id,
            message.text.len(),
            message.media.len()
        ));
        self.turn_for(&seat, &prompt, &images).await;
        self.run_leftovers(&seat).await;
    }

    /// What the last turn on the seat was handed and never read — it
    /// failed — runs as a turn of its own, once, so a turn that keeps
    /// failing does not spin; what that one leaves is reported, not
    /// run. Nothing runs while the gateway is stopping: a turn under
    /// a cancelled token would record the message and answer nothing.
    /// Nothing runs on a seat the chat has left behind either — `/new`
    /// replaced it, and the old conversation is over.
    async fn run_leftovers(&self, seat: &Arc<crate::driver::Seat>) {
        let leftover = self.driver.take_undelivered(seat);
        if leftover.is_empty() {
            return;
        }
        let key = &seat.key;
        let closed = !self
            .driver
            .seat_by_key(key)
            .is_some_and(|current| Arc::ptr_eq(&current, seat));
        if self.cancel.is_cancelled() || closed {
            log(&format!(
                "{key}: {} steer(s) undelivered ({}): {:?}",
                leftover.len(),
                if closed {
                    "the chat started over"
                } else {
                    "at shutdown"
                },
                leftover.iter().map(|s| s.text.as_str()).collect::<Vec<_>>()
            ));
            return;
        }
        log(&format!(
            "{key}: {} undelivered steer(s) run now",
            leftover.len()
        ));
        let folded = fold_steers(leftover);
        self.turn_for(seat, &folded.text, &folded.images).await;
        let dropped = self.driver.take_undelivered(seat);
        if !dropped.is_empty() {
            let folded = fold_steers(dropped);
            log(&format!(
                "{key}: still undelivered, dropped: {:?}",
                folded.text
            ));
            self.deliver(
                &seat.channel,
                &seat.chat_id,
                &format!("I could not get to this message: {}", folded.text),
            )
            .await;
        }
    }

    /// One turn on a seat, with its status line and its reply, and the
    /// chat told when it could not run.
    async fn turn_for(
        &self,
        seat: &Arc<crate::driver::Seat>,
        prompt: &str,
        images: &[ImageContent],
    ) {
        let key = &seat.key;
        let outcome = self.driver.run(seat, prompt, images).await;
        match outcome {
            Ok(report) if report.outcome == ilar::agent::TurnOutcome::Aborted => {
                log(&format!("{key}: turn aborted"));
                self.keep_handovers(seat, &report);
                let asked_for = self.driver.abort_was_asked_for(seat);
                self.deliver(&seat.channel, &seat.chat_id, self.aborted_reply(asked_for))
                    .await;
            }
            Ok(report) => {
                self.keep_handovers(seat, &report);
                self.deliver_unless_sent(seat, &report).await;
                self.schedule_review(seat.clone());
            }
            Err(TurnError::Busy(why)) => {
                log(&format!("{key}: {why}"));
                self.deliver(&seat.channel, &seat.chat_id, BUSY_REPLY).await;
            }
            // The chat started over while this waited: the fresh chat
            // hears nothing about a conversation it did not have.
            Err(TurnError::Closed) => log(&format!("{key}: turn dropped, the chat started over")),
            Err(TurnError::Failed(error)) => {
                log(&format!("{key}: turn failed: {error:#}"));
                self.deliver(
                    &seat.channel,
                    &seat.chat_id,
                    &failed_reply("That turn", &error),
                )
                .await;
            }
        }
    }

    /// Take a message the person sent back out of the chat, by the id
    /// the channel gave it. `true` when it is gone for everyone.
    async fn delete_inbound(&self, message: &Inbound) -> bool {
        let Some(id) = &message.message_id else {
            return false;
        };
        let Some(channel) = self.channels.get(&message.channel) else {
            return false;
        };
        match channel.delete_message(&message.chat_id, id).await {
            Ok(gone) => gone,
            Err(error) => {
                log(&format!(
                    "{}: message {id} not deleted: {error:#}",
                    message.session_key()
                ));
                false
            }
        }
    }

    /// What a cancelled turn is told. The gateway stopping is not the
    /// person's `/abort`: their prompt was dropped mid-flight and
    /// nothing re-runs it, so the reply says to send it again.
    fn aborted_reply(&self, asked_for: bool) -> &'static str {
        aborted_reply(asked_for, self.cancel.is_cancelled())
    }

    /// `/grant`, `/password` or `/deny`: the ask standing on this
    /// chat's seat gets the answer, and the chat hears what was decided.
    fn answer_ask(&self, key: &str, answer: crate::grants::Answer, ask: Option<&str>) -> String {
        match self.driver.seat_by_key(key) {
            Some(seat) => match self.driver.answer_ask(&seat, answer, ask) {
                Ok(text) => {
                    log(&format!("{key}: {text}"));
                    text
                }
                Err(why) => why.to_string(),
            },
            None if ask.is_some() => crate::grants::STALE_BUTTON.to_string(),
            None => "Nothing is waiting for a grant.".to_string(),
        }
    }

    /// Open the seat of a chat the routes still know, for a console
    /// command that reads it: seats live in memory, so after a restart
    /// every chat had "no session" until it next said something, though
    /// its session was on disk and named in the routes.
    async fn reopen_known_seat(&self, key: &str, message: &Inbound) {
        if self.driver.seat_by_key(key).is_some()
            || self.routes.snapshot().session_for(key).is_none()
        {
            return;
        }
        if let Err(error) = self
            .driver
            .seat(key, &message.channel, &message.chat_id, message.is_group)
            .await
        {
            log(&format!("{key}: cannot reopen the session: {error:#}"));
        }
    }

    /// `/status`: what this chat's seat is and is doing. Nothing is
    /// opened for it — a chat that has not spoken has nothing to show.
    fn status_reply(&self, key: &str) -> String {
        use crate::driver::Activity;
        let Some(seat) = self.driver.seat_by_key(key) else {
            return "No session open for this chat yet — say something first.".to_string();
        };
        let model = self
            .driver
            .current_model(&seat)
            .unwrap_or_else(|_| "unknown".to_string());
        let turn = match self.driver.activity(&seat) {
            Activity::Idle => "idle".to_string(),
            Activity::Turn(elapsed) => format!("running for {}", human_duration(elapsed)),
            Activity::Compacting => "compacting".to_string(),
        };
        let running = seat.runtime.spawner.running_tasks().len();
        let held = seat
            .runtime
            .spawner
            .undelivered_results(&seat.runtime.session_id)
            .len();
        let mut lines = vec![
            format!("Model: {model}"),
            format!("Turn: {turn}"),
            // What `ilar --view` takes to watch this chat from a terminal.
            format!("Session: {}", seat.runtime.session_id),
        ];
        lines.push(match (running, held) {
            (0, 0) => "Subagents: none".to_string(),
            (n, 0) => format!("Subagents: {n} running"),
            (n, h) => format!("Subagents: {n} running, {h} result(s) held for delivery"),
        });
        if let Some(ask) = self.driver.waiting_on(&seat) {
            lines.push(format!("Waiting on: {ask}"));
        }
        // The committed log, read without a stamp check: a turn may be
        // writing it this moment, and the last turn's usage is there
        // either way.
        let context = seat
            .runtime
            .store
            .audit_events(&seat.runtime.session_id)
            .ok()
            .and_then(|events| {
                events.iter().rev().find_map(|event| match event {
                    ilar::session::SessionEvent::AssistantMessage { usage, .. } => {
                        Some(usage.context_tokens())
                    }
                    _ => None,
                })
            });
        lines.push(match context {
            Some(tokens) => format!(
                "Context: about {} tokens after the last turn",
                with_commas(tokens)
            ),
            None => "Context: no turn yet".to_string(),
        });
        lines.join("\n")
    }

    /// `/cost`: the session's spend over its whole log, priced where
    /// the model is priced.
    fn cost_reply(&self, key: &str) -> String {
        use ilar::session::{SessionEvent, Usage};
        let Some(seat) = self.driver.seat_by_key(key) else {
            return "No session open for this chat yet — nothing spent.".to_string();
        };
        let events = match seat.runtime.store.whole_events(&seat.runtime.session_id) {
            Ok(events) => events,
            Err(error) => return failed_reply("/cost", &anyhow::anyhow!(error)),
        };
        let mut by_model: std::collections::BTreeMap<String, Usage> =
            std::collections::BTreeMap::new();
        for event in &events {
            if let SessionEvent::AssistantMessage { model, usage, .. } = event {
                let total = by_model.entry(model.clone()).or_default();
                total.input_tokens += usage.input_tokens;
                total.output_tokens += usage.output_tokens;
                total.cache_read_input_tokens += usage.cache_read_input_tokens;
                total.cache_creation_input_tokens += usage.cache_creation_input_tokens;
            }
        }
        if by_model.is_empty() {
            return "Nothing spent yet.".to_string();
        }
        // `input_tokens` is the uncached part: the prompt as the model
        // saw it is that plus what was read from the cache plus what
        // was written to it, and each is billed at its own rate.
        let (mut input, mut cached, mut written, mut output) = (0, 0, 0, 0);
        let mut dollars = 0.0;
        let mut unpriced = Vec::new();
        for (model, usage) in &by_model {
            input += usage.input_tokens
                + usage.cache_read_input_tokens
                + usage.cache_creation_input_tokens;
            cached += usage.cache_read_input_tokens;
            written += usage.cache_creation_input_tokens;
            output += usage.output_tokens;
            match ilar::model::pricing_for(model) {
                Some(pricing) => dollars += pricing.cost(usage),
                None => unpriced.push(model.as_str()),
            }
        }
        let models = by_model.keys().cloned().collect::<Vec<_>>().join(", ");
        let cost = if unpriced.len() == by_model.len() {
            format!("Cost: not priced ({models})")
        } else if unpriced.is_empty() {
            format!("Cost: ${dollars:.2} ({models})")
        } else {
            format!(
                "Cost: ${dollars:.2}, not counting {} which is not priced",
                unpriced.join(", ")
            )
        };
        format!(
            "Tokens: {} in ({} read from the cache, {} written to it) · {} out\n{cost}",
            with_commas(input),
            with_commas(cached),
            with_commas(written),
            with_commas(output)
        )
    }

    /// `/cron`: the jobs addressed to this chat — and, in a private
    /// chat, the gateway's own, which go to the last private chat heard
    /// from — or one of them removed by id or unique name. Adding stays
    /// with the model's tool: a person adds by asking.
    fn cron_reply(&self, key: &str, is_group: bool, remove: Option<&str>) -> String {
        use crate::cron::{LAST_ACTIVE, Schedule};
        let jobs: Vec<crate::cron::Job> = self
            .cron
            .list()
            .into_iter()
            .filter(|job| job.target == key || (!is_group && job.target == LAST_ACTIVE))
            .collect();
        let describe = |job: &crate::cron::Job| {
            let when = match &job.schedule {
                Schedule::Cron { expr } => format!("cron {expr} (UTC)"),
                Schedule::Every { secs } => format!(
                    "every {}",
                    human_duration(std::time::Duration::from_secs(*secs))
                ),
                Schedule::At { at } => format!("once at {}", at.to_rfc3339()),
            };
            let next = job
                .next_run
                .map(|at| at.format("%Y-%m-%d %H:%M UTC").to_string())
                .unwrap_or_else(|| "never".to_string());
            format!("{} · {} — {when}, next {next}", job.id, job.name)
        };
        let Some(which) = remove else {
            if jobs.is_empty() {
                return "No jobs scheduled for this chat. Ask for one — \"remind me at nine\" — and the model schedules it."
                    .to_string();
            }
            return jobs.iter().map(describe).collect::<Vec<_>>().join("\n");
        };
        let matching: Vec<&crate::cron::Job> = jobs
            .iter()
            .filter(|job| job.id == which || job.name.eq_ignore_ascii_case(which))
            .collect();
        match matching.as_slice() {
            [] => format!(
                "No job {which} here.{}",
                if jobs.is_empty() {
                    String::new()
                } else {
                    format!(
                        " Scheduled: {}",
                        jobs.iter().map(describe).collect::<Vec<_>>().join("; ")
                    )
                }
            ),
            // The gateway's own job comes back at the next start while
            // its setting is on; a removal that lasted until then
            // would be a lie.
            [job] if job.id == crate::weekly::JOB_ID => format!(
                "{} is the gateway's own: [gateway.weekly] enabled = false turns it off.",
                job.name
            ),
            [job] => match self.cron.remove(&job.id) {
                Ok(true) => {
                    log(&format!(
                        "{key}: job {} ({}) removed from the chat",
                        job.name, job.id
                    ));
                    format!("Removed {} ({}).", job.name, job.id)
                }
                Ok(false) => format!("{} was already gone.", job.name),
                Err(error) => failed_reply("/cron remove", &error),
            },
            several => format!(
                "{which} names {} jobs; remove by id: {}",
                several.len(),
                several
                    .iter()
                    .map(|job| job.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    /// `/tasks`: the subagents working for this chat, and the results
    /// waiting to be delivered to it.
    fn tasks_reply(&self, key: &str) -> String {
        let Some(seat) = self.driver.seat_by_key(key) else {
            return "No session open for this chat yet.".to_string();
        };
        let running = seat.runtime.spawner.running_tasks();
        let held = seat
            .runtime
            .spawner
            .undelivered_results(&seat.runtime.session_id);
        if running.is_empty() && held.is_empty() {
            return "No subagents running for this chat, and nothing held for delivery."
                .to_string();
        }
        let mut lines = Vec::new();
        for task in &running {
            lines.push(format!(
                "{} {}: {} — {}{}",
                if task.delivering {
                    "delivering to"
                } else {
                    "running"
                },
                task.agent,
                task.description,
                human_duration(task.started.elapsed()),
                if task.background {
                    ", in the background"
                } else {
                    ""
                }
            ));
        }
        for result in &held {
            lines.push(format!(
                "held for delivery: {}{}",
                result.description,
                if result.is_error { " (failed)" } else { "" }
            ));
        }
        lines.join("\n")
    }

    /// What is staged, one line each, or that nothing is.
    fn pending_listing(&self) -> String {
        match self.pending.list() {
            Ok(list) if list.is_empty() => "Nothing pending.".to_string(),
            Ok(list) => list
                .iter()
                .map(|p| format!("{} — {}", p.id, p.plan.describe().join("; ")))
                .collect::<Vec<_>>()
                .join("\n"),
            Err(error) => failed_reply("/pending", &error),
        }
    }

    /// `/approve` or `/reject` for an id that is not staged: say what
    /// is, since the ids are short and easy to mistype.
    fn nothing_pending_as(&self, id: &str) -> String {
        match self.pending.list() {
            Ok(list) if list.is_empty() => "Nothing pending.".to_string(),
            Ok(list) => format!(
                "Nothing pending as {id}. Pending: {}",
                list.iter()
                    .map(|p| p.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Err(_) => format!("Nothing pending as {id}."),
        }
    }

    /// A slash command, answered by the gateway itself.
    async fn command(&self, key: &str, message: &Inbound, command: Command) -> String {
        // Whatever else it was, if a password came in on it then it is
        // in the chat's history now: out it comes first, and every arm
        // below says whether that worked.
        let taken_back = command.carries_a_secret() && self.delete_inbound(message).await;
        if message.is_group
            && let Some((name, why)) = command.private_only()
        {
            let advice = if command.carries_a_secret() {
                format!(
                    " {} Others here may have read it first.",
                    maybe_password_advice(taken_back)
                )
            } else {
                String::new()
            };
            return format!("/{name} works only in a private chat with me: it {why}.{advice}");
        }
        match command {
            Command::Help => commands::HELP.to_string(),
            // Every Telegram chat opens with a START tap, and it used to be
            // answered "No command /start." with the whole help under it.
            Command::Start => "Hello. Write to me as you would to a person; /help lists the \
                               commands."
                .to_string(),
            Command::Pending => self.pending_listing(),
            // Bare: which one? Acting on all of them is one tap away on
            // Telegram, and not something to do by accident.
            Command::Approve(None) | Command::Reject(None) => {
                let verb = if matches!(command, Command::Approve(_)) {
                    "approve"
                } else {
                    "reject"
                };
                match self.pending_listing().as_str() {
                    "Nothing pending." => "Nothing pending.".to_string(),
                    listing => format!("{listing}\nWhich one? /{verb} <id>, or /{verb} all."),
                }
            }
            Command::Approve(Some(id)) => match self.pending.take(&id) {
                Ok(taken) if taken.is_empty() => self.nothing_pending_as(&id),
                Ok(taken) => taken
                    .iter()
                    .map(|p| p.plan.apply(&self.memory, &self.skills))
                    .collect::<crate::review::Applied>()
                    .report(),
                Err(error) => failed_reply("/approve", &error),
            },
            Command::Reject(Some(id)) => match self.pending.take(&id) {
                Ok(taken) if taken.is_empty() => self.nothing_pending_as(&id),
                // What was dropped, not how many: an id nobody reads
                // means nothing a week later.
                Ok(taken) => format!(
                    "Dropped: {}",
                    taken
                        .iter()
                        .flat_map(|p| p.plan.describe())
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
                Err(error) => failed_reply("/reject", &error),
            },
            Command::Unknown(name) => format!("No command /{name}.\n{}", commands::HELP),
            // A misspelt `/unlock` or `/password` ran nothing, so say
            // that first: the store is still sealed, or the ask is
            // still standing, and the person is waiting on it. The help
            // goes under it as it does under any unknown command — the
            // guess may be wrong, and then the list is what was wanted.
            // In a room "send it again" would be refused: say where to.
            Command::MistypedSecret { typed, meant } => format!(
                "No command /{typed} — did you mean /{meant}? Nothing ran; send it again{}. {}\n{}",
                if message.is_group {
                    " in a private chat with me"
                } else {
                    ""
                },
                maybe_password_advice(taken_back),
                commands::HELP
            ),
            // A password typed after `/grant` answers nothing, and it
            // is in the chat's history all the same: said as such, and
            // already taken back out above.
            Command::Misread(text) if text == commands::PASSWORD_AFTER_THE_YES => {
                format!("{text} {}", password_advice(taken_back))
            }
            Command::Misread(message) => message,
            Command::Compact => {
                use ilar::compaction::ManualCompactionOutcome as Outcome;
                let seat = match self
                    .driver
                    .seat(key, &message.channel, &message.chat_id, message.is_group)
                    .await
                {
                    Ok(seat) => seat,
                    Err(error) => {
                        log(&format!("{key}: /compact failed: {error:#}"));
                        return failed_reply("/compact", &error);
                    }
                };
                match self.driver.compact(&seat).await {
                    Ok(Outcome::Compacted {
                        summary,
                        context_tokens,
                    }) => {
                        log(&format!("{key}: compacted from the chat"));
                        self.note_handover(&seat, &summary);
                        format!(
                            "Compacted. The conversation is now one handover of {} characters, \
                             about {context_tokens} tokens of context.",
                            summary.chars().count()
                        )
                    }
                    Ok(Outcome::NothingToCompact) => "Nothing to compact yet.".to_string(),
                    Ok(Outcome::Aborted) => "Compaction aborted.".to_string(),
                    Err(error) => {
                        log(&format!("{key}: /compact failed: {error:#}"));
                        failed_reply("/compact", &error)
                    }
                }
            }
            Command::Grant { grant, ask } => {
                self.answer_ask(key, crate::grants::Answer::Grant(grant), ask.as_deref())
            }
            Command::Deny { ask } => {
                self.answer_ask(key, crate::grants::Answer::No, ask.as_deref())
            }
            Command::Password(password) => {
                // In the chat's history the moment it was sent, right
                // ask or wrong: taken back out above, and said either
                // way — as `/unlock` does.
                let verdict = self.answer_ask(key, crate::grants::Answer::Password(password), None);
                format!("{verdict} {}", password_advice(taken_back))
            }
            Command::Usage(usage) => usage.to_string(),
            Command::Status => {
                self.reopen_known_seat(key, message).await;
                self.status_reply(key)
            }
            Command::Cost => {
                self.reopen_known_seat(key, message).await;
                self.cost_reply(key)
            }
            Command::Cron { remove } => self.cron_reply(key, message.is_group, remove.as_deref()),
            Command::Tasks => {
                self.reopen_known_seat(key, message).await;
                self.tasks_reply(key)
            }
            Command::Whoami => format!(
                "Sender {} in chat {} on {} — allow_from takes the sender; the session key is {key}.",
                message.sender_id, message.chat_id, message.channel
            ),
            Command::Restart => {
                log(&format!("{key}: restart asked from the chat"));
                self.restart
                    .store(true, std::sync::atomic::Ordering::Release);
                // After this reply has left: the stop closes the
                // outbound queue behind whatever is in it.
                let me = self.clone_handle();
                tokio::spawn(async move {
                    tokio::time::sleep(RESTART_GRACE).await;
                    me.cancel();
                });
                "Restarting: turns in flight are stopped, and the service starts the gateway again."
                    .to_string()
            }
            Command::Unlock(password) => {
                // Right password or wrong, it is in the chat's history
                // now: taken back out above, and said either way.
                let store = self.driver.secret_store();
                let verdict = if !store.is_sealed() {
                    "The secret store is not sealed; nothing to unlock.".to_string()
                } else if !store.is_locked() {
                    "The secret store is already unlocked.".to_string()
                } else {
                    match store.unlock(&password) {
                        Ok(()) => {
                            log(&format!("{key}: secret store unlocked"));
                            "Secret store unlocked for this gateway process.".to_string()
                        }
                        Err(error) => failed_reply("/unlock", &error),
                    }
                };
                format!("{verdict} {}", password_advice(taken_back))
            }
            Command::Abort => match self.driver.seat_by_key(key) {
                Some(seat) if self.driver.abort(&seat) => {
                    log(&format!("{key}: turn aborted from the chat"));
                    "Aborting the running turn.".to_string()
                }
                _ => "Nothing is running.".to_string(),
            },
            Command::New => match self.driver.close(key).await {
                // A room has no memory to keep, so it is not promised one.
                Ok(()) if message.is_group => {
                    format!("Started a fresh chat on {}.", self.driver.default_model())
                }
                Ok(()) => format!(
                    "Started a fresh chat on {}. What I remember about you stays.",
                    self.driver.default_model()
                ),
                Err(error) => {
                    log(&format!("{key}: /new failed: {error:#}"));
                    failed_reply("/new", &error)
                }
            },
            Command::Model { model: None, save } => {
                let current = match self
                    .driver
                    .seat(key, &message.channel, &message.chat_id, message.is_group)
                    .await
                {
                    Ok(seat) => self.driver.chosen_model(&seat),
                    Err(error) => Err(error),
                };
                if save {
                    return match current.and_then(|model| {
                        self.driver.save_default_model(&model)?;
                        Ok(model)
                    }) {
                        Ok(model) => format!("{model} is the default for new chats now."),
                        Err(error) => {
                            log(&format!("{key}: /model --save failed: {error:#}"));
                            failed_reply("/model --save", &error)
                        }
                    };
                }
                let current = current.ok();
                // Grouped by provider, one line each, so the list fits
                // what a chat shows without folding.
                let mut by_provider: std::collections::BTreeMap<String, Vec<String>> =
                    std::collections::BTreeMap::new();
                for model in self.driver.available_models() {
                    let (provider, id) = model.split_once('/').unwrap_or(("", &model));
                    by_provider
                        .entry(provider.to_string())
                        .or_default()
                        .push(id.to_string());
                }
                let mut lines = vec![
                    format!("Current: {}", current.unwrap_or_else(|| "unknown".into())),
                    format!("Default for new chats: {}", self.driver.default_model()),
                ];
                for (provider, ids) in by_provider {
                    lines.push(format!("{provider}: {}", ids.join(", ")));
                }
                lines.push(
                    "/model <provider/model> switches; add --save to make it the default."
                        .to_string(),
                );
                lines.join("\n")
            }
            Command::Model {
                model: Some(model),
                save,
            } => {
                let seat = match self
                    .driver
                    .seat(key, &message.channel, &message.chat_id, message.is_group)
                    .await
                {
                    Ok(seat) => seat,
                    Err(error) => {
                        log(&format!("{key}: /model failed: {error:#}"));
                        return failed_reply("/model", &error);
                    }
                };
                let switched = match self.driver.set_model(&seat, &model) {
                    Ok(ModelSwitch::Applied) => format!("Switched to {model}."),
                    Ok(ModelSwitch::Pending) => format!(
                        "Switched to {model} from your next message on; the turn running now keeps its model."
                    ),
                    Err(error) => return format!("{error:#}"),
                };
                if !save {
                    return switched;
                }
                match self.driver.save_default_model(&model) {
                    Ok(()) => format!("{switched} It is the default for new chats now."),
                    Err(error) => {
                        log(&format!("{key}: /model --save failed: {error:#}"));
                        format!("{switched} Not saved as the default: {error:#}")
                    }
                }
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
        let outcome = self.driver.run(&seat, &follow_up.prompt, &[]).await;
        // A message may have steered this turn too; whatever it never
        // read must not wait for the next one.
        let seat_for_leftovers = seat.clone();
        match outcome {
            // Cancelled from the chat: the report was not delivered, so
            // its outbox entry stays for the next start.
            Ok(report) if report.outcome == ilar::agent::TurnOutcome::Aborted => {
                log(&format!("{key}: follow-up aborted"));
                self.keep_handovers(&seat, &report);
                if !seat.background {
                    // Not the person's prompt: the report's outbox entry
                    // stays, and the next start delivers it.
                    let reply = if self.cancel.is_cancelled() {
                        RESTARTING_FOLLOW_UP
                    } else {
                        ABORTED_REPLY
                    };
                    self.deliver(&seat.channel, &seat.chat_id, reply).await;
                }
            }
            Ok(report) => {
                ilar::outbox::retire(&self.driver.outbox_dir(), &follow_up.retire);
                self.keep_handovers(&seat, &report);
                self.deliver_unless_sent(&seat, &report).await;
                self.schedule_review(seat.clone());
            }
            // A background seat's troubles stay in the log: the chat
            // never asked it anything.
            Err(TurnError::Busy(why)) => {
                log(&format!("{key}: follow-up waits: {why}"));
                if !seat.background {
                    self.deliver(&seat.channel, &seat.chat_id, BUSY_FOLLOW_UP)
                        .await;
                }
                self.driver.requeue(follow_up);
            }
            // The chat started over: nowhere to deliver it any more,
            // and the outbox entry keeps it for the next start.
            Err(TurnError::Closed) => {
                log(&format!("{key}: follow-up dropped, the chat started over"));
            }
            Err(TurnError::Failed(error)) => {
                log(&format!("{key}: follow-up failed: {error:#}"));
                if !seat.background {
                    self.deliver(
                        &seat.channel,
                        &seat.chat_id,
                        &failed_reply("Delivering a subagent's report", &error),
                    )
                    .await;
                }
            }
        }
        self.run_leftovers(&seat_for_leftovers).await;
    }

    /// Everything whose time has come: due cron jobs, and a heartbeat
    /// for every configured chat whose interval has passed.
    fn due_now(&self, last_heartbeat: &mut HashMap<String, Instant>) -> Vec<Due> {
        let mut due = Vec::new();
        match self.cron.take_due(chrono::Utc::now()) {
            Ok(jobs) => {
                for job in jobs {
                    due.push(Due {
                        key: job.session_key(),
                        name: job.name.clone(),
                        target: job.target.clone(),
                        prompt: job.prompt.clone(),
                        job: Some(job),
                    });
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
                    due.push(Due {
                        key: format!("heartbeat:{chat}"),
                        name: "heartbeat".to_string(),
                        target: chat.clone(),
                        prompt: heartbeat.prompt.clone(),
                        job: None,
                    });
                }
            }
        }
        due
    }

    /// A cron or heartbeat turn: its own session, homed on the chat it
    /// is for, and heard from only through the message tool.
    async fn handle_scheduled(&self, due: Due) {
        let Due {
            key,
            name,
            target,
            mut prompt,
            job,
        } = due;
        // The gateway's own job goes to whoever was last heard from in
        // private — its prompt reads the person's memory aloud, which
        // is not for a room — and has the sweep's findings appended.
        let target = if target == crate::cron::LAST_ACTIVE {
            match self
                .routes
                .snapshot()
                .last_private_chat()
                .map(str::to_string)
            {
                Some(last) => last,
                None => {
                    log(&format!("{key}: no private chat has written yet; skipped"));
                    return;
                }
            }
        } else {
            target
        };
        if key == format!("cron:{}", crate::weekly::JOB_ID) {
            match crate::weekly::sweep(&self.skills, chrono::Utc::now(), &self.settings.weekly) {
                Ok(sweep) => {
                    let report = sweep.report();
                    if !report.is_empty() {
                        log(&format!("{key}: {report}"));
                        prompt.push(' ');
                        prompt.push_str(&report);
                    }
                }
                Err(error) => log(&format!("{key}: sweep failed: {error:#}")),
            }
        }
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
                self.job_failed(&key, &name, channel, chat_id, &format!("{error:#}"), job)
                    .await;
                return;
            }
        };
        log(&format!("{key}: scheduled turn for {target}"));
        match self.driver.run(&seat, &prompt, &[]).await {
            Ok(report) => {
                self.keep_handovers(&seat, &report);
                if report.sent == 0 {
                    log(&format!("{key}: nothing to say"));
                } else {
                    log(&format!("{key}: {} message(s) sent", report.sent));
                }
            }
            Err(error) => {
                log(&format!("{key}: scheduled turn failed: {error}"));
                self.job_failed(&key, &name, channel, chat_id, &error.to_string(), job)
                    .await;
            }
        }
    }

    /// A job that could not run: silence is the wrong answer to
    /// somebody's reminder, so the chat is told, and a one-shot —
    /// which `take_due` has already retired — gets one more go. A
    /// heartbeat has no job and stays silent: nobody asked for it.
    async fn job_failed(
        &self,
        key: &str,
        name: &str,
        channel: &str,
        chat_id: &str,
        cause: &str,
        job: Option<crate::cron::Job>,
    ) {
        let Some(job) = job else {
            return;
        };
        self.deliver(
            channel,
            chat_id,
            &failed_line(&format!("Job {name}"), cause),
        )
        .await;
        self.retry_once(key, job);
    }

    /// A one-shot that failed never fired at all: it is scheduled once
    /// more, shortly, and if that fails too it is done — a reminder
    /// that keeps failing must not become a loop.
    fn retry_once(&self, key: &str, job: crate::cron::Job) {
        if job.retries > 0 || !matches!(job.schedule, crate::cron::Schedule::At { .. }) {
            return;
        }
        let now = chrono::Utc::now();
        let again = crate::cron::Job {
            id: ilar::session::new_id()[..8].to_string(),
            schedule: crate::cron::Schedule::At {
                at: now + ONE_SHOT_RETRY,
            },
            next_run: None,
            retries: job.retries + 1,
            ..job
        };
        match self.cron.add(again, now) {
            Ok(added) => log(&format!(
                "{key}: {} runs again at {}",
                added.name,
                added.next_run.map(|at| at.to_rfc3339()).unwrap_or_default()
            )),
            Err(error) => log(&format!("{key}: not scheduled again: {error:#}")),
        }
    }

    /// After a turn, arrange the review: it runs once the seat has been
    /// quiet for the idle window, only if no later turn reset that
    /// window, and only if the episode was worth it.
    fn schedule_review(&self, seat: Arc<crate::driver::Seat>) {
        // A room is not reviewed: what is said there is not the user's
        // to remember, and the core memory is withheld from it too.
        if !self.settings.review.enabled || seat.background || !seat.private {
            return;
        }
        let generation = seat
            .review_generation
            .load(std::sync::atomic::Ordering::Acquire);
        let idle = self.driver.review_idle(&seat);
        let gateway = self.clone_handle();
        let cancel = self.cancel.child_token();
        tokio::spawn(async move {
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(idle) => {}
            }
            if seat
                .review_generation
                .load(std::sync::atomic::Ordering::Acquire)
                != generation
            {
                return;
            }
            gateway.review(&seat).await;
        });
    }

    /// The review itself: an aside over the conversation, its answer a
    /// plan applied through the memory store or staged for approval,
    /// and one line to the chat about what was kept.
    async fn review(&self, seat: &crate::driver::Seat) {
        let episode = seat.episode.lock().unwrap().clone();
        // The model kept its own memory: it already answered the
        // question this asks. The episode goes with it — left standing
        // it would suppress every later review on this seat too.
        if episode.wrote_memory {
            *seat.episode.lock().unwrap() = crate::review::Episode::default();
            log(&format!(
                "{}: review skipped: the model kept its own memory",
                seat.key
            ));
            return;
        }
        if !episode.worth_reviewing(self.settings.review.min_tool_calls) {
            return;
        }
        let answer = match self
            .driver
            .aside(seat, crate::review::PROMPT.as_str())
            .await
        {
            Ok(Some(answer)) => answer,
            Ok(None) => return,
            Err(error) => {
                log(&format!("{}: review failed: {error:#}", seat.key));
                return;
            }
        };
        let plan = match crate::review::Answer::parse(&answer) {
            crate::review::Answer::Nothing => {
                *seat.episode.lock().unwrap() = crate::review::Episode::default();
                log(&format!("{}: review: nothing to keep", seat.key));
                return;
            }
            crate::review::Answer::Unparsed => {
                // The episode stays: the next review sees it again.
                log(&format!(
                    "{}: review answered without a plan: {}",
                    seat.key,
                    answer.trim()
                ));
                return;
            }
            crate::review::Answer::Plan(plan) => plan,
        };
        *seat.episode.lock().unwrap() = crate::review::Episode::default();
        if self.settings.review.approval {
            match self.pending.stage(&seat.key, plan.clone()) {
                Ok(staged) => {
                    let lines = plan.describe().join("; ");
                    self.deliver_with_buttons(
                        &seat.channel,
                        &seat.chat_id,
                        &format!(
                            "📝 I would remember: {lines} — /approve {} or /reject {}",
                            staged.id, staged.id
                        ),
                        vec![
                            crate::bus::Button::new("Remember", &format!("/approve {}", staged.id)),
                            crate::bus::Button::new("Drop", &format!("/reject {}", staged.id)),
                        ],
                    )
                    .await;
                }
                Err(error) => log(&format!("{}: review not staged: {error:#}", seat.key)),
            }
            return;
        }
        let report = plan.apply(&self.memory, &self.skills).report();
        log(&format!("{}: review: {report}", seat.key));
        self.deliver(&seat.channel, &seat.chat_id, &report).await;
    }

    /// A compaction's handover is the turn's own summary of what it
    /// was doing; the daily note keeps it, since the session log it
    /// came from is not what a future session reads.
    fn keep_handovers(&self, seat: &crate::driver::Seat, report: &TurnReport) {
        for summary in &report.compactions {
            self.note_handover(seat, summary);
        }
    }

    /// One handover into the daily note, when memory is on.
    fn note_handover(&self, seat: &crate::driver::Seat, summary: &str) {
        if !self.settings.memory.enabled {
            return;
        }
        let heading = format!("handover in {}", seat.key);
        if let Err(error) = self.memory.daily(chrono::Utc::now(), &heading, summary) {
            log(&format!("{}: daily note not written: {error:#}", seat.key));
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
        // A turn that ends with nothing — a model that spent its whole
        // output thinking, say — must not end in silence: the chat is
        // told, and the log has it.
        if report.text.trim().is_empty() {
            log(&format!(
                "{}: the turn ended without a reply ({:?})",
                seat.key, report.outcome
            ));
            self.deliver(&seat.channel, &seat.chat_id, EMPTY_REPLY)
                .await;
            return;
        }
        self.deliver(&seat.channel, &seat.chat_id, &report.text)
            .await;
    }

    /// One outbound message, to its channel. Only a dispatcher lane
    /// calls this, one message at a time per channel. The status line is not touched
    /// here: the turn that owns it takes it down when it ends, and the
    /// message tool when its reply to its own chat goes out, so a
    /// scheduled job posting mid-turn no longer clears a line the
    /// person is still watching.
    async fn send(&self, message: Outbound) {
        let key = session_key(&message.channel, &message.chat_id);
        let Some(target) = self.channels.get(&message.channel) else {
            log(&format!("{key}: no such channel; dropping a message"));
            return;
        };
        let retry = Duration::from_secs(self.settings.send_retry_secs);
        for attempt in 1..=SEND_TRIES {
            // A channel whose server just died is back in a few
            // seconds; a message it refused is worth another try
            // before anyone is told it did not go.
            match target.send(message.clone()).await {
                Ok(()) => return,
                Err(error) if attempt == SEND_TRIES => {
                    log(&format!(
                        "{key}: send failed after {attempt} tries: {error:#}"
                    ));
                    self.undelivered(&key, target, &message, &error).await;
                }
                Err(error) => {
                    log(&format!("{key}: send failed ({error:#}); trying again"));
                    tokio::select! {
                        () = self.cancel.cancelled() => {
                            log(&format!("{key}: stopping; that message went nowhere"));
                            return;
                        }
                        () = tokio::time::sleep(retry) => {}
                    }
                }
            }
        }
    }

    /// A message the channel would not take, once the tries are spent.
    /// The model was told "sent to …" the moment it queued it, and the
    /// chat is silent, so both are told: the chat directly, with the
    /// text it never got, and the seat's next turn through its prompt.
    async fn undelivered(
        &self,
        key: &str,
        target: &Arc<dyn Channel>,
        message: &Outbound,
        error: &anyhow::Error,
    ) {
        let what = failed_reply("Delivering a message", error);
        self.driver.note_undelivered(key, &what);
        // Sent straight, once, and without the media that may be what
        // the channel refused: the queue is where we already are.
        let notice = Outbound {
            channel: message.channel.clone(),
            chat_id: message.chat_id.clone(),
            text: format!("⚠ {what}\nWhat it said: {}", message.text),
            media: Vec::new(),
            buttons: Vec::new(),
        };
        if let Err(error) = target.send(notice).await {
            log(&format!(
                "{key}: the chat could not be told either: {error:#}"
            ));
        }
    }

    /// The chat the gateway announces itself to: the person's last
    /// private chat, if any. A start, a stop and a script's
    /// report are the person's business, not a room's.
    fn announce_target(&self) -> Option<(String, String)> {
        let routes = self.routes.snapshot();
        let last = routes.last_private_chat()?;
        split_key(last).map(|(channel, chat)| (channel.to_string(), chat.to_string()))
    }

    /// One line to the last private chat once the gateway is up. The
    /// channel may still be connecting, so a failed send is tried
    /// again for a while; a chat that has never written gets nothing.
    async fn announce_start(&self) {
        let Some((channel, chat_id)) = self.announce_target() else {
            return;
        };
        let Some(target) = self.channels.get(&channel).cloned() else {
            return;
        };
        let text = format!(
            "▶ {} started · default model {}",
            build_line(),
            self.driver.default_model()
        );
        for attempt in 1..=ANNOUNCE_TRIES {
            let message = Outbound {
                channel: channel.clone(),
                chat_id: chat_id.clone(),
                text: text.clone(),
                media: Vec::new(),
                buttons: Vec::new(),
            };
            match target.send(message).await {
                Ok(()) => {
                    log(&format!("{channel}:{chat_id}: start announced"));
                    return;
                }
                Err(error) if attempt == ANNOUNCE_TRIES => {
                    log(&format!(
                        "{channel}:{chat_id}: start not announced: {error:#}"
                    ));
                }
                Err(_) => {
                    tokio::select! {
                        () = self.cancel.cancelled() => return,
                        () = tokio::time::sleep(ANNOUNCE_RETRY) => {}
                    }
                }
            }
        }
    }

    /// One line as the gateway goes down, sent directly: the queue is
    /// about to close and the channels with it.
    async fn announce_stop(&self) {
        let Some((channel, chat_id)) = self.announce_target() else {
            return;
        };
        let Some(target) = self.channels.get(&channel) else {
            return;
        };
        let message = Outbound {
            channel: channel.clone(),
            chat_id: chat_id.clone(),
            text: "⏹ ilar-gateway stopping".into(),
            media: Vec::new(),
            buttons: Vec::new(),
        };
        match target.send(message).await {
            Ok(()) => {
                log(&format!("{channel}:{chat_id}: stop announced"));
                // A channel may only have queued it: give the wire a
                // moment before the channel goes down with the process.
                tokio::time::sleep(ANNOUNCE_SETTLE).await;
            }
            Err(error) => log(&format!(
                "{channel}:{chat_id}: stop not announced: {error:#}"
            )),
        }
    }

    /// Something the gateway says on the model's behalf, through the
    /// same queue as the model's own sends, so it never overtakes them.
    async fn deliver(&self, channel: &str, chat_id: &str, text: &str) {
        self.deliver_with_buttons(channel, chat_id, text, Vec::new())
            .await;
    }

    /// The same, with answers the person can tap where the channel
    /// shows buttons.
    async fn deliver_with_buttons(
        &self,
        channel: &str,
        chat_id: &str,
        text: &str,
        buttons: Vec<crate::bus::Button>,
    ) {
        if text.trim().is_empty() {
            return;
        }
        // Whole: a channel that folds long texts splits them itself.
        let message = Outbound {
            channel: channel.to_string(),
            chat_id: chat_id.to_string(),
            text: text.to_string(),
            media: Vec::new(),
            buttons,
        };
        if self.outbound_tx.send(message).await.is_err() {
            log(&format!(
                "{channel}:{chat_id}: the dispatcher is gone; dropping a reply"
            ));
        }
    }

    /// Messages left by `ilar-gateway notify`: rate-limited per source,
    /// addressed explicitly or to the last private chat.
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
            let target = message.to.clone().or_else(|| {
                self.routes
                    .snapshot()
                    .last_private_chat()
                    .map(str::to_string)
            });
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
                    sender_id: inbox::sender(&message.source),
                    sender_name: None,
                    // Nothing in a chat to delete: the gateway wrote it.
                    message_id: None,
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

/// What arrived with a message: images the model can see, and a line
/// per attachment naming its path, so a file it cannot see it can
/// still `read`, and an image it can see it can still open.
pub fn attachments(media: &[PathBuf]) -> (Vec<ImageContent>, String) {
    let mut images = Vec::new();
    let mut notes = String::new();
    for path in media {
        // Weighed before it is read: anyone who can message the bot
        // can attach anything, and a file is held whole the moment it
        // is read. Over the cap it is named rather than carried, which
        // is what happens to a non-image of any size anyway.
        let weight = std::fs::metadata(path).map(|file| file.len()).ok();
        if let Some(bytes) = weight
            && bytes > ilar::image::MAX_IMAGE_FILE_BYTES
        {
            notes.push_str(&format!(
                "\n(file attached: {}, {bytes} bytes — too large to view; read it if it matters)",
                path.display(),
            ));
            continue;
        }
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) => {
                notes.push_str(&format!(
                    "\n(attachment {} unreadable: {error})",
                    path.display()
                ));
                continue;
            }
        };
        match ilar::image::from_file_bytes(&bytes) {
            Some(image) => {
                images.push(image);
                notes.push_str(&format!("\n(image attached, also at {})", path.display()));
            }
            None => notes.push_str(&format!(
                "\n(file attached: {}, {} bytes — read it if it matters)",
                path.display(),
                bytes.len()
            )),
        }
    }
    (images, notes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_names_what_and_why_on_one_clipped_line() {
        let error = anyhow::anyhow!("model refused\nstatus 429").context("provider error");
        assert_eq!(
            failed_reply("That turn", &error),
            "That turn failed: provider error: model refused status 429"
        );
        let long = anyhow::anyhow!("{}", "x".repeat(1000));
        let reply = failed_reply("/new", &long);
        assert!(reply.chars().count() <= "/new failed: ".len() + FAILURE_CAUSE_CHARS);
        assert!(reply.ends_with('…'));
    }

    /// A channel that cannot take the message back says so, rather
    /// than leaving the person to assume it is gone.
    #[test]
    fn the_password_advice_depends_on_whether_the_message_went() {
        assert!(password_advice(true).starts_with("I deleted"));
        assert!(password_advice(false).starts_with("Delete that message"));
    }
}
