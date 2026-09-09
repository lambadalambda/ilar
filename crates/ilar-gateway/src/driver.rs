//! Turns on the library runtime, one live runtime per chat.
//!
//! A chat's runtime stays open between turns: its background subagents
//! keep running, and their completions come back to the chat as
//! follow-up turns the way they reach a TUI. Turns on one chat are
//! serialized; different chats run at once.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use ilar::agent::{
    LOOP_EVENT_CAPACITY, LoopEvent, Steer, SteerReceiver, SteerSender, TurnOutcome,
    loop_event_channel, steer_channel,
};
use ilar::config::Config;
use ilar::delivery::{Disposition, Parcel, disposition};
use ilar::provider::ProviderResolver;
use ilar::runtime::{RuntimeOptions, RuntimePlan, SessionRuntime};
use ilar::session::{ImageContent, SessionStore};
use ilar::subagent::{Notification, SubagentSpawner};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::bus::Outbound;
use crate::config::GatewayConfig;
use crate::cron::{CronStore, CronTool};
use crate::memory::{MemoryGetTool, MemorySearchTool, MemoryStore, MemoryTool};
use crate::message::MessageTool;
use crate::routes::RouteStore;

/// What a turn produced: the streamed text, and how the loop ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnReport {
    pub session_id: String,
    pub text: String,
    pub outcome: TurnOutcome,
    /// Messages the model sent through its tool during this turn.
    /// Counted under the seat's lock, so another turn's sends are
    /// never mistaken for this one's.
    pub sent: usize,
    /// Handover summaries the turn compacted into, oldest first: what
    /// the daily note keeps.
    pub compactions: Vec<String>,
}

#[derive(Debug)]
pub enum TurnError {
    /// The session's writer is held elsewhere — a TUI has it open.
    Busy(String),
    Failed(anyhow::Error),
}

impl std::fmt::Display for TurnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy(why) => write!(f, "session busy: {why}"),
            Self::Failed(error) => write!(f, "{error:#}"),
        }
    }
}

/// When a model switch takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSwitch {
    /// Recorded now.
    Applied,
    /// Recorded when the running turn ends; the next turn uses it.
    Pending,
}

/// A child's completion owed to a root session as a prompt. `retire`
/// is the outbox entry the prompt settles — the completion itself, or
/// the stranded one a salvage speaks for — and is retired only once
/// the log holds the prompt.
#[derive(Debug, Clone)]
pub struct FollowUp {
    pub session_key: String,
    pub prompt: String,
    pub retire: Notification,
}

/// One chat's live runtime.
pub struct Seat {
    pub key: String,
    pub channel: String,
    pub chat_id: String,
    /// A cron or heartbeat seat: it speaks to its chat only through
    /// the message tool, never by its final text.
    pub background: bool,
    /// A private chat, not a room: the only kind that is reviewed and
    /// that gets the core memory.
    pub private: bool,
    /// A model switch asked for while a turn was running: applied
    /// when the next turn takes the lock.
    pending_model: Mutex<Option<String>>,
    /// What happened since the last review.
    pub episode: Mutex<crate::review::Episode>,
    /// Bumped by every turn; a scheduled review runs only if no turn
    /// came after the one that scheduled it.
    pub review_generation: std::sync::atomic::AtomicU64,
    pub runtime: SessionRuntime,
    /// Messages the model sent through its tool, ever; a turn compares
    /// before and after to know whether its final text is still owed.
    pub sent: Arc<std::sync::atomic::AtomicUsize>,
    turn: tokio::sync::Mutex<()>,
    /// The running turn's way in for a message that arrives meanwhile;
    /// `None` while the seat is idle.
    steer: Mutex<Option<SteerSender>>,
    /// Steers handed to the turn and not yet read by the model. Cleared
    /// as the loop reports each one delivered; whatever is left when
    /// the turn ends is the caller's to run again.
    undelivered: Mutex<Vec<Steer>>,
}

/// The channels a driver talks through, and what it knows about the
/// channels it talks for.
pub struct Wiring {
    pub follow_ups: mpsc::Sender<FollowUp>,
    /// Where the message tool sends; the gateway dispatches to channels.
    pub outbound: mpsc::Sender<Outbound>,
    /// Each channel's delivery constraints, for the tool's description.
    pub constraints: HashMap<String, String>,
    pub cron: Arc<CronStore>,
    pub memory: Arc<MemoryStore>,
    pub skills: Arc<crate::skills::SkillLibrary>,
}

pub struct Driver {
    config: Config,
    gateway: GatewayConfig,
    resolver: Arc<dyn ProviderResolver>,
    routes: Arc<RouteStore>,
    seats: Mutex<HashMap<String, Arc<Seat>>>,
    /// Opening a seat is slow file work; two messages racing on a
    /// fresh chat must not each create a session.
    opening: tokio::sync::Mutex<()>,
    wiring: Wiring,
    cancel: CancellationToken,
}

impl Driver {
    pub fn new(
        config: Config,
        gateway: GatewayConfig,
        resolver: Arc<dyn ProviderResolver>,
        routes: Arc<RouteStore>,
        wiring: Wiring,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            config,
            gateway,
            resolver,
            routes,
            seats: Mutex::new(HashMap::new()),
            opening: tokio::sync::Mutex::new(()),
            wiring,
            cancel,
        }
    }

    pub fn outbox_dir(&self) -> PathBuf {
        self.config.state_dir().join("outbox")
    }

    pub fn seat_by_key(&self, key: &str) -> Option<Arc<Seat>> {
        self.seats.lock().unwrap().get(key).cloned()
    }

    /// The chat's runtime, opened on first use: its session resumed
    /// when the routes name one that still exists, created otherwise.
    pub async fn seat(
        &self,
        key: &str,
        channel: &str,
        chat_id: &str,
        is_group: bool,
    ) -> Result<Arc<Seat>> {
        self.seat_of(key, channel, chat_id, false, !is_group).await
    }

    /// A scheduled turn's runtime: its own session under `key`, homed
    /// on the chat it is addressed to.
    pub async fn background_seat(
        &self,
        key: &str,
        channel: &str,
        chat_id: &str,
    ) -> Result<Arc<Seat>> {
        let private = !self
            .routes
            .snapshot()
            .is_group(&crate::bus::session_key(channel, chat_id));
        self.seat_of(key, channel, chat_id, true, private).await
    }

    async fn seat_of(
        &self,
        key: &str,
        channel: &str,
        chat_id: &str,
        background: bool,
        private: bool,
    ) -> Result<Arc<Seat>> {
        if let Some(seat) = self.seat_by_key(key) {
            return Ok(seat);
        }
        let _opening = self.opening.lock().await;
        if let Some(seat) = self.seat_by_key(key) {
            return Ok(seat);
        }
        let routes = self.routes.snapshot();
        let known = if background {
            routes.background_session_for(key)
        } else {
            routes.session_for(key)
        }
        .map(str::to_string);
        let mut runtime = match self.open(known.clone(), private) {
            Ok(runtime) => runtime,
            // A route to a session that is gone (deleted, another state
            // dir) is a route to nothing: start over rather than refuse
            // the chat forever.
            Err(error) if known.is_some() => {
                log(&format!(
                    "{key}: session {} unusable ({error:#}); starting a new one",
                    known.unwrap_or_default()
                ));
                self.open(None, private)?
            }
            Err(error) => return Err(error),
        };
        // A background session is nobody's address: it is remembered
        // in its own map, where the message and cron tools never look.
        self.routes.update(|routes| {
            if background {
                routes.bind_background(key, &runtime.session_id);
            } else {
                routes.bind(key, &runtime.session_id);
            }
        })?;
        // The model's way to answer: a tool that knows this chat.
        let (tool, sent) = MessageTool::new(
            self.wiring.outbound.clone(),
            channel,
            chat_id,
            self.routes.clone(),
            self.gateway.workspace(&self.config),
            self.wiring
                .constraints
                .get(channel)
                .map(String::as_str)
                .unwrap_or(""),
        );
        runtime.registry.add(tool)?;
        if self.gateway.memory.enabled {
            let memory = self.wiring.memory.clone();
            for (name, tool) in [
                (
                    "memory",
                    MemoryTool::new(memory.clone()) as Arc<dyn ilar::tools::Tool>,
                ),
                ("memory_search", MemorySearchTool::new(memory.clone())),
                ("memory_get", MemoryGetTool::new(memory)),
            ] {
                if self.gateway.tools.admits(name) {
                    runtime.registry.add(tool)?;
                }
            }
        }
        // Its own skills, to write as well as read.
        if self.gateway.tools.admits("skill_manage") {
            runtime.registry.add(crate::skills::SkillManageTool::new(
                self.wiring.skills.clone(),
            ))?;
        }
        // And its calendar, unless the policy says otherwise.
        if self.gateway.tools.admits("cron") {
            let home = crate::bus::session_key(channel, chat_id);
            runtime.registry.add(CronTool::new(
                self.wiring.cron.clone(),
                self.routes.clone(),
                &home,
            ))?;
        }
        let seat = Arc::new(Seat {
            key: key.to_string(),
            channel: channel.to_string(),
            chat_id: chat_id.to_string(),
            background,
            private,
            pending_model: Mutex::new(None),
            episode: Mutex::new(crate::review::Episode::default()),
            review_generation: std::sync::atomic::AtomicU64::new(0),
            runtime,
            sent,
            turn: tokio::sync::Mutex::new(()),
            steer: Mutex::new(None),
            undelivered: Mutex::new(Vec::new()),
        });
        tokio::spawn(watch_notifications(
            seat.runtime.spawner.clone(),
            seat.runtime.store.clone(),
            self.outbox_dir(),
            seat.runtime.session_id.clone(),
            key.to_string(),
            self.wiring.follow_ups.clone(),
            self.cancel.child_token(),
        ));
        self.seats
            .lock()
            .unwrap()
            .insert(key.to_string(), seat.clone());
        Ok(seat)
    }

    fn open(&self, resume: Option<String>, private: bool) -> Result<SessionRuntime> {
        let plan = plan(
            &self.config,
            &self.gateway,
            &self.wiring.memory,
            resume,
            private,
        )?;
        let policy = &self.gateway.tools;
        let mut runtime = plan.start_with(&self.config, self.resolver.clone())?;
        if !policy.is_empty() {
            let admitted = policy.admit(runtime.registry.tool_names());
            let registry = std::mem::replace(
                &mut runtime.registry,
                ilar::tools::ToolRegistry::read_only(),
            );
            runtime.registry = registry.restricted_to(&admitted);
        }
        Ok(runtime)
    }

    /// One turn on a seat; a second caller waits for the first. Status
    /// lines, when wanted, go to `status` as the turn moves.
    pub async fn run(
        &self,
        seat: &Seat,
        prompt: &str,
        images: &[ImageContent],
        status: Option<mpsc::UnboundedSender<String>>,
    ) -> std::result::Result<TurnReport, TurnError> {
        let _turn = seat.turn.lock().await;
        let pending = seat.pending_model.lock().unwrap().take();
        if let Some(model) = pending {
            self.persist_model(seat, &model)
                .map_err(TurnError::Failed)?;
        }
        let sent_before = seat.sent.load(std::sync::atomic::Ordering::Acquire);
        seat.review_generation
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let mut narrator = crate::status::Narrator::default();
        let mut watch = crate::skills::SkillWatch::default();
        let skills = self.wiring.skills.clone();
        let observe = move |event: &LoopEvent| {
            seat.episode.lock().unwrap().observe(event);
            if let Some(name) = watch.observe(event) {
                let _ = skills.note_view(&name);
            }
            if let LoopEvent::Steered { text, .. } = event {
                let mut undelivered = seat.undelivered.lock().unwrap();
                if let Some(at) = undelivered.iter().position(|steer| steer.text == *text) {
                    undelivered.remove(at);
                }
            }
            if let Some(status) = &status
                && let Some(line) = narrator.observe(event)
            {
                let _ = status.send(line);
            }
        };
        let (steer_tx, steer_rx) = steer_channel();
        *seat.steer.lock().unwrap() = Some(steer_tx);
        let outcome = turn(
            &seat.runtime,
            prompt,
            images,
            self.cancel.child_token(),
            observe,
            Some(steer_rx),
        )
        .await;
        *seat.steer.lock().unwrap() = None;
        let mut report = outcome?;
        report.sent = seat.sent.load(std::sync::atomic::Ordering::Acquire) - sent_before;
        Ok(report)
    }

    /// Hand a message to the turn running on the seat, the way typing
    /// into the TUI mid-turn does: the loop reads it at its next step.
    /// `false` when no turn is running, or it is just ending — then
    /// the message is a turn of its own.
    pub fn steer(&self, seat: &Seat, text: &str, images: &[ImageContent]) -> bool {
        if seat.turn.try_lock().is_ok() {
            return false;
        }
        let steer = Steer {
            text: text.to_string(),
            images: images.to_vec(),
        };
        let sender = seat.steer.lock().unwrap().clone();
        let Some(sender) = sender else {
            return false;
        };
        // Recorded before the send, so the loop's report of delivery
        // cannot arrive first and find nothing to clear.
        seat.undelivered.lock().unwrap().push(steer.clone());
        if sender.send(steer).is_err() {
            seat.undelivered.lock().unwrap().pop();
            return false;
        }
        true
    }

    /// Steers the last turn on the seat never delivered.
    pub fn take_undelivered(&self, seat: &Seat) -> Vec<Steer> {
        std::mem::take(&mut *seat.undelivered.lock().unwrap())
    }

    pub fn seats(&self) -> Vec<Arc<Seat>> {
        self.seats.lock().unwrap().values().cloned().collect()
    }

    /// Close a chat's seat and forget its route: the next message
    /// opens a fresh session. The old session stays on disk. Whatever
    /// the seat was running is stopped first.
    pub async fn close(&self, key: &str) -> Result<()> {
        let seat = self.seats.lock().unwrap().remove(key);
        if let Some(seat) = seat {
            seat.runtime.spawner.shutdown().await;
            seat.runtime.services.stop_all();
        }
        self.routes.update(|routes| routes.unbind(key))
    }

    /// Every model this configuration can reach, as `provider/id`.
    pub fn available_models(&self) -> Vec<String> {
        self.config
            .available_models()
            .iter()
            .map(|model| model.full_id())
            .collect()
    }

    /// The model a seat's next turn runs on: a switch asked for during
    /// a running turn counts, since that is what the next turn takes.
    pub fn chosen_model(&self, seat: &Seat) -> Result<String> {
        if let Some(pending) = seat.pending_model.lock().unwrap().clone() {
            return Ok(pending);
        }
        self.current_model(seat)
    }

    /// The model the seat's session records.
    pub fn current_model(&self, seat: &Seat) -> Result<String> {
        Ok(seat
            .runtime
            .store
            .load(&seat.runtime.session_id)?
            .effective_model())
    }

    fn known_model(&self, model: &str) -> Result<()> {
        if !self.available_models().iter().any(|known| known == model) {
            bail!("no model {model}; /model lists them");
        }
        Ok(())
    }

    /// A question over the seat's conversation, answered without
    /// recording anything and served from the cached prefix — the
    /// review's vehicle. `None` when a turn holds the seat or the seat
    /// has no conversation yet.
    pub async fn aside(&self, seat: &Seat, question: &str) -> Result<Option<String>> {
        let Ok(_idle) = seat.turn.try_lock() else {
            return Ok(None);
        };
        ilar::aside::ask(
            seat.runtime.resolver.as_ref(),
            &seat.runtime.store,
            &seat.runtime.session_id,
            Some(&seat.runtime.system_prompt),
            &seat.runtime.registry.definitions(),
            question,
            &self.cancel.child_token(),
        )
        .await
    }

    /// How long a seat should be quiet before its review runs: just
    /// before the provider's cache window closes, unless configured.
    pub fn review_idle(&self, seat: &Seat) -> std::time::Duration {
        if let Some(secs) = self.gateway.review.after_idle_secs {
            return std::time::Duration::from_secs(secs);
        }
        let provider = self
            .current_model(seat)
            .ok()
            .and_then(|model| {
                model
                    .split_once('/')
                    .map(|(provider, _)| provider.to_string())
            })
            .unwrap_or_default();
        let ttl = self.config.cache_compact.ttl_for(&provider);
        let margin = std::time::Duration::from_secs(self.config.cache_compact.margin_secs);
        ttl.checked_sub(margin).unwrap_or(ttl / 2)
    }

    /// The model a fresh chat starts on: the one saved from a chat
    /// with `/model … --save`, else `gateway.model`, else the core's.
    pub fn default_model(&self) -> String {
        default_model(&self.config, &self.gateway)
    }

    /// Make `model` the default for new chats, in `<home>/model`.
    pub fn save_default_model(&self, model: &str) -> Result<()> {
        self.known_model(model)?;
        crate::routes::write_atomically(
            &self.gateway.home(&self.config).join(SAVED_MODEL_FILE),
            format!("{model}\n").as_bytes(),
        )
    }

    /// Switch a seat's session to `model`. Recorded now when the seat
    /// is idle; when a turn is running, recorded as that turn ends, so
    /// the answer never waits behind it.
    pub fn set_model(&self, seat: &Seat, model: &str) -> Result<ModelSwitch> {
        self.known_model(model)?;
        match seat.turn.try_lock() {
            Ok(_idle) => {
                self.persist_model(seat, model)?;
                Ok(ModelSwitch::Applied)
            }
            Err(_) => {
                *seat.pending_model.lock().unwrap() = Some(model.to_string());
                Ok(ModelSwitch::Pending)
            }
        }
    }

    fn persist_model(&self, seat: &Seat, model: &str) -> Result<()> {
        ilar::runtime::persist_model_change(
            self.resolver.as_ref(),
            &seat.runtime.store,
            &seat.runtime.session_id,
            model,
            None,
        )
        .map(drop)
    }

    /// Hand a follow-up back after a wait — the seat's writer was held
    /// — without holding up whoever asked.
    pub fn requeue(&self, follow_up: FollowUp) {
        let sender = self.wiring.follow_ups.clone();
        let cancel = self.cancel.child_token();
        tokio::spawn(async move {
            tokio::select! {
                () = cancel.cancelled() => {}
                () = tokio::time::sleep(HOLD_RETRY) => { let _ = sender.send(follow_up).await; }
            }
        });
    }

    /// Stop every seat's background work and services, all at once:
    /// each shutdown waits out an abort grace, and seats are many.
    pub async fn shutdown(&self) {
        let seats = self.seats();
        futures::future::join_all(seats.iter().map(|seat| async {
            seat.runtime.spawner.shutdown().await;
            seat.runtime.services.stop_all();
        }))
        .await;
    }
}

/// Where `/model … --save` keeps the default for new chats.
pub const SAVED_MODEL_FILE: &str = "model";

/// The saved default, then the configured one, then the core's.
fn default_model(config: &Config, gateway: &GatewayConfig) -> String {
    std::fs::read_to_string(gateway.home(config).join(SAVED_MODEL_FILE))
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|model| !model.is_empty())
        .or_else(|| gateway.model.clone())
        .unwrap_or_else(|| config.general.model.clone())
}

/// The session plan a gateway chat runs on: the assistant's workspace,
/// agent and model, a SOUL.md before any coding instructions, the tool
/// policy narrowed into every agent definition, and — for a private
/// chat — the core memory frozen into the system prompt.
pub fn plan(
    config: &Config,
    gateway: &GatewayConfig,
    memory: &MemoryStore,
    resume: Option<String>,
    private: bool,
) -> Result<RuntimePlan> {
    let workspace = gateway.workspace(config);
    let home = gateway.home(config);
    std::fs::create_dir_all(&workspace)
        .with_context(|| format!("creating workspace {}", workspace.display()))?;
    let mut plan = RuntimePlan::resolve(
        config,
        &RuntimeOptions {
            // A resumed session keeps the model it records, so a
            // `/model` switch outlives a restart; the default is for
            // fresh sessions only.
            model: resume.is_none().then(|| default_model(config, gateway)),
            agent: gateway.agent.clone(),
            resume,
            cwd: workspace.clone(),
            // Nobody sits at a channel to answer a form: the tool is
            // left off and the model is told so on the spot.
            questions: false,
            project_instructions: None,
            // An assistant has a SOUL.md before it has coding
            // instructions: who it is, how it talks — and reads it,
            // its skills and its agents from its own home, not from
            // the terminal agent's configuration.
            context_files: Some(ilar::config::SOUL_FILES),
            user_dir: Some(home.clone()),
            // Its own skills only: not the built-ins, not the working
            // directory's.
            own_skills_only: true,
        },
    )?;
    // Where it is: reached over a chat, with a home, wakeable from a
    // script. Before the memory, which is about the person.
    plan.system_prompt.push_str("\n\n");
    plan.system_prompt
        .push_str(&crate::situation::block(&home, &workspace));
    // The policy reaches the subagents too: an agent definition's
    // own restriction is narrowed before the spawner is built from
    // it, and the chat's registry is filtered after.
    let policy = &gateway.tools;
    if !policy.is_empty() {
        let nameable: Vec<&'static str> = ilar::tools::ToolRegistry::builtin()
            .tool_names()
            .into_iter()
            .chain(ilar::tools::child_tool_names())
            .collect();
        for agent in &mut plan.agents {
            agent.tools = policy.narrow(agent.tools.as_deref(), nameable.iter().copied());
        }
        plan.agent.tools = policy.narrow(plan.agent.tools.as_deref(), nameable.iter().copied());
    }
    // The core memory rides in the system prompt, frozen for the
    // session, and never into a group: what the assistant knows
    // about its person is not for a room.
    if private
        && gateway.memory.enabled
        && let Some(block) = memory.core_block()?
    {
        plan.system_prompt.push_str("\n\n");
        plan.system_prompt.push_str(&block);
    }
    Ok(plan)
}

/// Run one turn and keep its text. The events are the loop's own; this
/// driver reads the answer and lets the rest go by.
pub async fn turn(
    runtime: &SessionRuntime,
    prompt: &str,
    images: &[ImageContent],
    cancel: CancellationToken,
    mut observe: impl FnMut(&LoopEvent),
    steer: Option<SteerReceiver>,
) -> std::result::Result<TurnReport, TurnError> {
    let (events, mut rx) = loop_event_channel(LOOP_EVENT_CAPACITY);
    let turn = ilar::agent::run_turn(
        runtime.resolver.as_ref(),
        &runtime.registry,
        &runtime.store,
        &runtime.session_id,
        prompt,
        images,
        Some(&runtime.system_prompt),
        runtime.loop_config.clone(),
        events,
        cancel,
        runtime.tool_ctx.clone(),
        steer,
    );
    tokio::pin!(turn);
    let mut text = String::new();
    let mut compactions = Vec::new();
    let mut note = |event: LoopEvent| {
        observe(&event);
        match event {
            LoopEvent::TextDelta(delta) => text.push_str(&delta),
            LoopEvent::Compacted { summary, .. } => compactions.push(summary),
            _ => {}
        }
    };
    let outcome = loop {
        tokio::select! {
            event = rx.recv() => match event {
                Some(event) => note(event),
                None => break (&mut turn).await,
            },
            outcome = &mut turn => break outcome,
        }
    };
    while let Ok(event) = rx.try_recv() {
        note(event);
    }
    match outcome {
        Ok(outcome) => Ok(TurnReport {
            session_id: runtime.session_id.clone(),
            text,
            outcome,
            sent: 0,
            compactions,
        }),
        Err(error) if ilar::agent::TurnNeverStarted::writer_held(&error) => {
            Err(TurnError::Busy(format!("{error:#}")))
        }
        Err(error) => Err(TurnError::Failed(error)),
    }
}

/// Everything a seat's subagents publish, from the outbox first (what a
/// previous process left undelivered) and then live. A completion for
/// the seat's own session becomes a follow-up turn; one for a child is
/// routed down the tree; one that cannot be delivered at all reaches
/// the chat as an error rather than vanishing.
async fn watch_notifications(
    spawner: Arc<SubagentSpawner>,
    store: SessionStore,
    outbox_dir: PathBuf,
    session_id: String,
    key: String,
    follow_ups: mpsc::Sender<FollowUp>,
    cancel: CancellationToken,
) {
    let mut live = spawner.subscribe();
    let mut queue: VecDeque<Parcel> = ilar::outbox::pending(&store, &outbox_dir, &session_id)
        .into_iter()
        .map(Parcel::fresh)
        .collect();
    let mut held: Vec<Parcel> = Vec::new();
    // A fixed deadline, not a fresh sleep per iteration: a steady
    // trickle of live notifications must not starve the held ones.
    let mut retry_at: Option<tokio::time::Instant> = None;
    loop {
        while let Some(parcel) = queue.pop_front() {
            let notification = parcel.notification().clone();
            if notification.parent_session_id == session_id {
                let follow_up = FollowUp {
                    session_key: key.clone(),
                    prompt: notification.text.clone(),
                    retire: notification,
                };
                if follow_ups.send(follow_up).await.is_err() {
                    return;
                }
                continue;
            }
            let routed = spawner
                .route_notification(notification, cancel.child_token())
                .await;
            match disposition(routed, parcel) {
                Disposition::Delivered => {}
                Disposition::Propagate(next) => queue.push_back(next),
                Disposition::Hold(parcel) => {
                    held.push(parcel);
                    retry_at.get_or_insert_with(|| tokio::time::Instant::now() + HOLD_RETRY);
                }
                Disposition::Exhausted(stranded) => {
                    salvage(&follow_ups, &key, stranded, "no session left to climb to").await;
                }
                Disposition::Salvage {
                    notification,
                    error,
                } => salvage(&follow_ups, &key, notification, &error).await,
            }
        }
        let retry = async {
            match retry_at {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            () = cancel.cancelled() => return,
            next = live.recv() => match next {
                Some(notification) => queue.push_back(Parcel::fresh(notification)),
                None => return,
            },
            () = retry => {
                retry_at = None;
                queue.extend(held.drain(..));
            }
        }
    }
}

/// How long a held delivery waits before the next attempt.
pub const HOLD_RETRY: std::time::Duration = std::time::Duration::from_secs(5);

/// The delivery of last resort: the child's report goes to the chat's
/// own session as an error prompt. The stranded outbox entry rides
/// along and is retired only once that prompt is in the log.
async fn salvage(
    follow_ups: &mpsc::Sender<FollowUp>,
    key: &str,
    stranded: Notification,
    reason: &str,
) {
    let prompt = format!(
        "<task-notification>\nTask \"{}\" finished but its result could not be delivered to the session that asked for it ({reason}). Its report:\n\n{}\n</task-notification>",
        stranded.description, stranded.text
    );
    let _ = follow_ups
        .send(FollowUp {
            session_key: key.to_string(),
            prompt,
            retire: stranded,
        })
        .await;
}

pub fn log(message: &str) {
    eprintln!("{} {message}", chrono::Local::now().format("%H:%M:%S"));
}
