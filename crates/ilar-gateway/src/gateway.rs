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

use crate::bus::{Inbound, Outbound, session_key, split_for_delivery, split_key};
use crate::channel::Channel;
use crate::commands::{self, Command};
use crate::config::GatewayConfig;
use crate::cron::CronStore;
use crate::driver::{Driver, FollowUp, ModelSwitch, TurnError, TurnReport, Wiring, log};
use crate::inbox::{self, RateLimit};
use crate::memory::MemoryStore;
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
    memory: Arc<MemoryStore>,
    skills: Arc<crate::skills::SkillLibrary>,
    pending: crate::review::PendingStore,
    settings: GatewayConfig,
    /// The gateway's own handle, for tasks it spawns on itself.
    me: std::sync::Weak<Gateway>,
    /// The status line each chat is watching, while a turn runs there.
    statuses: Mutex<HashMap<String, Status>>,
    cancel: CancellationToken,
}

/// A posted status line and the task that keeps it current.
struct Status {
    id: String,
    updater: tokio::task::JoinHandle<()>,
}

/// What a chat is told when its session is held by another process.
pub const BUSY_REPLY: &str =
    "This chat's session is open somewhere else (a TUI, most likely); try again when it is closed.";
/// The same, for a subagent's report that has to wait.
pub const BUSY_FOLLOW_UP: &str = "A subagent finished, but this chat's session is open somewhere else; its report is delivered once that closes.";
/// What a chat is told when something failed: what, and the cause,
/// clipped. The chat is allowlisted to its operator, who wants the
/// reason where they are; the full chain is in the log regardless.
pub fn failed_reply(what: &str, error: &anyhow::Error) -> String {
    let cause: String = format!("{error:#}")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
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

/// How long a stop waits for turns in flight before giving up on them.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(15);
/// The pause before a channel that stopped is started again.
const CHANNEL_RESTART: Duration = Duration::from_secs(5);
/// How long the start announcement keeps trying while the channel
/// connects, and how long the stop announcement may take.
const ANNOUNCE_RETRY: Duration = Duration::from_secs(2);
const ANNOUNCE_TRIES: u32 = 30;
const ANNOUNCE_GRACE: Duration = Duration::from_secs(5);

/// What the start line says about this build.
pub fn build_line() -> String {
    let commit = env!("ILAR_GATEWAY_COMMIT");
    if commit.is_empty() {
        format!("ilar-gateway {}", env!("CARGO_PKG_VERSION"))
    } else {
        format!("ilar-gateway {} ({commit})", env!("CARGO_PKG_VERSION"))
    }
}
/// The longest text the gateway itself sends in one message: a chat
/// folds anything much longer behind a "[...]".
const DELIVERY_PIECE_CHARS: usize = 700;

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
            },
            cancel.clone(),
        ));
        Ok(Arc::new_cyclic(|me| Self {
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
            memory,
            skills,
            pending: crate::review::PendingStore::new(dir.join("pending")),
            settings,
            statuses: Mutex::new(HashMap::new()),
            me: me.clone(),
            cancel,
        }))
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
            tokio::spawn(async move {
                while let Some(message) = outbound.recv().await {
                    gateway.send(message).await;
                }
            })
        };
        let mut handlers = tokio::task::JoinSet::new();
        if self.settings.announce {
            let gateway = self.clone();
            handlers.spawn(async move { gateway.announce_start().await });
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
                    for (key, target, prompt) in self.due_now(&mut last_heartbeat) {
                        let gateway = self.clone();
                        handlers.spawn(async move { gateway.handle_scheduled(key, target, prompt).await });
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
        self.driver.shutdown().await;
        // The senders are gone with the seats; the dispatcher ends when
        // the queue is empty, so nothing said during the grace is lost.
        let _ = tokio::time::timeout(SHUTDOWN_GRACE, dispatcher).await;
        Ok(())
    }

    async fn handle_inbound(&self, message: Inbound) {
        let key = message.session_key();
        if let Some(command) = commands::parse(&message.text) {
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
        let prompt = format!("{}{notes}", message.text);
        log(&format!(
            "{key}: turn from {} ({} chars, {} attachment(s))",
            message.sender_id,
            message.text.len(),
            message.media.len()
        ));
        let status = self.begin_status(&seat).await;
        let outcome = self.driver.run(&seat, &prompt, &images, status).await;
        self.end_status(&key).await;
        match outcome {
            Ok(report) => {
                self.keep_handovers(&seat, &report);
                self.deliver_unless_sent(&seat, &report).await;
                self.schedule_review(seat.clone());
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
                    &failed_reply("That turn", &error),
                )
                .await;
            }
        }
    }

    /// A slash command, answered by the gateway itself.
    async fn command(&self, key: &str, message: &Inbound, command: Command) -> String {
        match command {
            Command::Help => commands::HELP.to_string(),
            Command::Pending => match self.pending.list() {
                Ok(list) if list.is_empty() => "Nothing pending.".to_string(),
                Ok(list) => list
                    .iter()
                    .map(|p| format!("{} ({}): {}", p.id, p.chat, p.plan.describe().join("; ")))
                    .collect::<Vec<_>>()
                    .join("\n"),
                Err(error) => failed_reply("/pending", &error),
            },
            Command::Approve(id) => match self.pending.take(&id) {
                Ok(taken) if taken.is_empty() => format!("Nothing pending as {id}."),
                Ok(taken) => {
                    let mut lines = Vec::new();
                    for p in taken {
                        lines.extend(p.plan.apply(&self.memory, &self.skills));
                    }
                    format!("💾 remembered: {}", lines.join("; "))
                }
                Err(error) => failed_reply("/approve", &error),
            },
            Command::Reject(id) => match self.pending.take(&id) {
                Ok(taken) if taken.is_empty() => format!("Nothing pending as {id}."),
                Ok(taken) => format!("Dropped {}.", taken.len()),
                Err(error) => failed_reply("/reject", &error),
            },
            Command::Unknown(name) => format!("No command /{name}.\n{}", commands::HELP),
            Command::New => match self.driver.close(key).await {
                Ok(()) => format!(
                    "Started a fresh chat on {}. What I remember about you stays.",
                    self.driver.default_model()
                ),
                Err(error) => {
                    log(&format!("{key}: /new failed: {error:#}"));
                    failed_reply("/new", &error)
                }
            },
            Command::Model(None) => {
                let current = match self
                    .driver
                    .seat(key, &message.channel, &message.chat_id, message.is_group)
                    .await
                {
                    Ok(seat) => self.driver.current_model(&seat).ok(),
                    Err(_) => None,
                };
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
                let mut lines = vec![format!(
                    "Current: {}",
                    current.unwrap_or_else(|| "unknown".into())
                )];
                for (provider, ids) in by_provider {
                    lines.push(format!("{provider}: {}", ids.join(", ")));
                }
                lines.push("/model <provider/model> switches.".to_string());
                lines.join("\n")
            }
            Command::Model(Some(model)) => {
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
                match self.driver.set_model(&seat, &model) {
                    Ok(ModelSwitch::Applied) => format!("Switched to {model}."),
                    Ok(ModelSwitch::Pending) => format!(
                        "Switched to {model} from your next message on; the turn running now keeps its model."
                    ),
                    Err(error) => format!("{error:#}"),
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
        let status = self.begin_status(&seat).await;
        let outcome = self.driver.run(&seat, &follow_up.prompt, &[], status).await;
        self.end_status(&key).await;
        match outcome {
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
    async fn handle_scheduled(&self, key: String, target: String, mut prompt: String) {
        // The gateway's own job goes to whoever was last heard from,
        // and has the sweep's findings appended.
        let target = if target == crate::cron::LAST_ACTIVE {
            match self.routes.snapshot().last_active {
                Some(last) => last,
                None => {
                    log(&format!("{key}: no chat has written yet; skipped"));
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
                return;
            }
        };
        log(&format!("{key}: scheduled turn for {target}"));
        match self.driver.run(&seat, &prompt, &[], None).await {
            Ok(report) => {
                self.keep_handovers(&seat, &report);
                if report.sent == 0 {
                    log(&format!("{key}: nothing to say"));
                } else {
                    log(&format!("{key}: {} message(s) sent", report.sent));
                }
            }
            Err(error) => log(&format!("{key}: scheduled turn failed: {error}")),
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
        if !episode.worth_reviewing(self.settings.review.min_tool_calls) {
            return;
        }
        let answer = match self.driver.aside(seat, crate::review::PROMPT).await {
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
                    self.deliver(
                        &seat.channel,
                        &seat.chat_id,
                        &format!(
                            "📝 I would remember: {lines} — /approve {} or /reject {}",
                            staged.id, staged.id
                        ),
                    )
                    .await;
                }
                Err(error) => log(&format!("{}: review not staged: {error:#}", seat.key)),
            }
            return;
        }
        let outcome = plan.apply(&self.memory, &self.skills);
        log(&format!("{}: review kept {}", seat.key, outcome.join("; ")));
        self.deliver(
            &seat.channel,
            &seat.chat_id,
            &format!("💾 remembered: {}", outcome.join("; ")),
        )
        .await;
    }

    /// A compaction's handover is the turn's own summary of what it
    /// was doing; the daily note keeps it, since the session log it
    /// came from is not what a future session reads.
    fn keep_handovers(&self, seat: &crate::driver::Seat, report: &TurnReport) {
        if !self.settings.memory.enabled {
            return;
        }
        for summary in &report.compactions {
            let heading = format!("handover in {}", seat.key);
            if let Err(error) = self.memory.daily(chrono::Utc::now(), &heading, summary) {
                log(&format!("{}: daily note not written: {error:#}", seat.key));
            }
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

    /// A status line in the chat for the turn about to run: "working…",
    /// then whatever the narrator says, edited no more often than the
    /// interval allows. `None` when the channel has no such thing, the
    /// setting is off, or the seat is a background one.
    async fn begin_status(
        &self,
        seat: &crate::driver::Seat,
    ) -> Option<mpsc::UnboundedSender<String>> {
        if !self.settings.status || seat.background {
            return None;
        }
        let channel = self.channels.get(&seat.channel)?.clone();
        let id = match channel
            .post_status(&seat.chat_id, crate::status::WORKING)
            .await
        {
            Ok(Some(id)) => id,
            Ok(None) => return None,
            Err(error) => {
                log(&format!("{}: status not posted: {error:#}", seat.key));
                return None;
            }
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let interval = Duration::from_secs(self.settings.status_interval_secs);
        let chat_id = seat.chat_id.clone();
        let status_id = id.clone();
        let updater = tokio::spawn(async move {
            let mut last_edit: Option<tokio::time::Instant> = None;
            let mut shown = crate::status::WORKING.to_string();
            while let Some(mut line) = rx.recv().await {
                // Wait out the interval, keeping only the newest line.
                if let Some(last) = last_edit {
                    let due = last + interval;
                    loop {
                        tokio::select! {
                            () = tokio::time::sleep_until(due) => break,
                            newer = rx.recv() => match newer {
                                Some(newer) => line = newer,
                                None => return,
                            },
                        }
                    }
                }
                if line == shown {
                    continue;
                }
                if let Err(error) = channel.edit_status(&chat_id, &status_id, &line).await {
                    log(&format!("status not edited: {error:#}"));
                    return;
                }
                shown = line;
                last_edit = Some(tokio::time::Instant::now());
            }
        });
        self.statuses
            .lock()
            .unwrap()
            .insert(seat.key.clone(), Status { id, updater });
        Some(tx)
    }

    /// Take the chat's status line down, if one is up.
    async fn end_status(&self, key: &str) {
        let status = self.statuses.lock().unwrap().remove(key);
        let Some(status) = status else {
            return;
        };
        status.updater.abort();
        let Some((channel, chat_id)) = split_key(key) else {
            return;
        };
        if let Some(target) = self.channels.get(channel)
            && let Err(error) = target.clear_status(chat_id, &status.id).await
        {
            log(&format!("{key}: status not cleared: {error:#}"));
        }
    }

    /// One outbound message, to its channel. Only the dispatcher calls
    /// this, one message at a time. The chat's status line goes first:
    /// the reply is what it was waiting for.
    async fn send(&self, message: Outbound) {
        let key = session_key(&message.channel, &message.chat_id);
        self.end_status(&key).await;
        let Some(target) = self.channels.get(&message.channel) else {
            log(&format!("{key}: no such channel; dropping a message"));
            return;
        };
        if let Err(error) = target.send(message).await {
            log(&format!("{key}: send failed: {error:#}"));
        }
    }

    /// The chat the gateway announces itself to: the last one heard
    /// from, if any.
    fn announce_target(&self) -> Option<(String, String)> {
        let last = self.routes.snapshot().last_active?;
        split_key(&last).map(|(channel, chat)| (channel.to_string(), chat.to_string()))
    }

    /// One line to the last active chat once the gateway is up. The
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
            "▶ {} started · model {}",
            build_line(),
            self.driver.default_model()
        );
        for attempt in 1..=ANNOUNCE_TRIES {
            let message = Outbound {
                channel: channel.clone(),
                chat_id: chat_id.clone(),
                text: text.clone(),
                media: Vec::new(),
            };
            match target.send(message).await {
                Ok(()) => return,
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
        };
        if let Err(error) = target.send(message).await {
            log(&format!(
                "{channel}:{chat_id}: stop not announced: {error:#}"
            ));
        }
    }

    /// Something the gateway says on the model's behalf, through the
    /// same queue as the model's own sends, so it never overtakes them.
    async fn deliver(&self, channel: &str, chat_id: &str, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        for piece in split_for_delivery(text, DELIVERY_PIECE_CHARS) {
            let message = Outbound {
                channel: channel.to_string(),
                chat_id: chat_id.to_string(),
                text: piece,
                media: Vec::new(),
            };
            if self.outbound_tx.send(message).await.is_err() {
                log(&format!(
                    "{channel}:{chat_id}: the dispatcher is gone; dropping a reply"
                ));
                return;
            }
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

/// What arrived with a message: images the model can see, and a line
/// per attachment naming its path, so a file it cannot see it can
/// still `read`, and an image it can see it can still open.
pub fn attachments(media: &[PathBuf]) -> (Vec<ImageContent>, String) {
    let mut images = Vec::new();
    let mut notes = String::new();
    for path in media {
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
}
