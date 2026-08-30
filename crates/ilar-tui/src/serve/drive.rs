//! The write path: `ilar serve` as a runtime, not only a reader.
//!
//! Phase 2 tailed the store. This drives it: the browser sends a
//! message, and the same headless loop `ilar exec` proved runs the turn
//! inside the server process. Nothing about the *turn* is new — it is
//! [`ilar::runtime::RuntimePlan`] resolved from the same configuration a
//! TUI launch resolves, then [`ilar::agent::run_turn`] — which is
//! exactly why streaming needed no new work: a driven turn writes the
//! session log and the `.live` scratch like any other turn, and the
//! watcher was already reading both.
//!
//! The runtime is the *session's*, not the turn's. The first turn this
//! process drives on a session resolves a [`SessionRuntime`] and keeps
//! it — the same lifetime the TUI gives it — so the spawner's background
//! children and the service manager's processes survive the turn that
//! started them. Each kept runtime gets the one subscriber of its
//! background-notification channel ([`Consumer`]): a completion for the
//! driven session becomes a follow-up turn through the same slot and
//! lease a web message takes, and a completion for a child's child is
//! routed with `route_notification` — each target session in its own
//! serial lane, so a delivery that can only wait does not hold the
//! others' mail. The consumer also starts by adopting the durable
//! outbox: completions an earlier process recorded for this tree and
//! never delivered re-enter here as if freshly notified. Teardown moved
//! with the lifetime: [`Drive::shutdown`], at process exit, is where
//! children are cancelled and services stopped.
//!
//! Three invariants hold this together:
//!
//! - **One turn per session, and the OS says so.** A session is driven
//!   only after this process takes its writer lease
//!   ([`SessionStore::acquire_writer`]); a session open in a TUI refuses
//!   it, and that refusal is the 409 the page shows as "watching only".
//!   The lease is held from the moment the decision is made until the
//!   spawned task hands it to `run_turn`, which acquires it itself —
//!   the handoff is a `drop` one line before the call rather than a
//!   window a second process could walk through.
//!
//!   Against *this* process the lease says nothing — a flock is per
//!   open file description, so a second request here would take it
//!   again — which is why the registry entry is claimed first, in one
//!   critical section with the steer-vs-start decision, and before the
//!   slow `plan()`. That claim is what a racing request sees, and it is
//!   held by a [`TurnSlot`] that gives it back on every path out,
//!   including a panic.
//! - **Steer-vs-start is one rule, in one place.** It is
//!   [`crate::decide::submit_target`], the TUI's own decision function,
//!   asked with the state serve can observe. A turn this process is
//!   driving takes the message at its next step; anything else starts a
//!   turn.
//! - **Nobody is here to answer.** Like `ilar exec`, a driven turn is
//!   built with `questions: false`, so the `question` tool is not in the
//!   registry at all and a model that reaches for it is told so on the
//!   spot instead of blocking on a human who is not in this process.
//!   An interactive answer modal in the browser is a follow-up.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::http::StatusCode;

use ilar::agent::{LOOP_EVENT_CAPACITY, SteerSender, loop_event_channel, steer_channel};
use ilar::config::Config;
use ilar::provider::ProviderResolver;
use ilar::runtime::{RuntimeOptions, RuntimePlan, SessionRuntime};
use ilar::session::{SessionStore, SessionWriter};
use ilar::delivery::Parcel;
use ilar::subagent::{Notification, RouteOutcome};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::decide::{self, LoopState, SubmitTarget};

/// What a write request did, as the page reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fate {
    /// A new turn is running for this message.
    Started,
    /// A turn was already running here and takes it at its next step.
    Steering,
    /// The running turn was cancelled.
    Aborted,
}

impl Fate {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Steering => "steering",
            Self::Aborted => "aborted",
        }
    }
}

/// Why a write request did nothing. Each carries the status the page
/// branches on: 409 in particular is not an error to log but a state to
/// render — the session belongs to another process and this tab is a
/// watcher.
#[derive(Debug, Clone)]
pub(crate) enum DriveError {
    /// Another process holds the writer lease.
    Locked,
    /// *This* process is between turns on that session — starting one,
    /// or unwinding one — and has nowhere to hold a message meanwhile.
    /// Deliberately not [`Self::Locked`]: 409 tells the page it is a
    /// watcher of somebody else's session, which is exactly what this is
    /// not, and the composer would lock over a state that clears itself
    /// in a moment.
    Busy(String),
    NotFound(String),
    /// Nothing here to abort.
    NotDriving,
    Invalid(String),
    Failed(String),
}

/// The words the page shows on a refused write. Said once, here, because
/// the banner quotes them verbatim.
pub(crate) const WATCHING_ONLY: &str = "session is open in another process — watching only";

impl DriveError {
    pub(crate) fn status(&self) -> StatusCode {
        match self {
            Self::Locked => StatusCode::CONFLICT,
            Self::Busy(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::NotFound(_) | Self::NotDriving => StatusCode::NOT_FOUND,
            Self::Invalid(_) => StatusCode::BAD_REQUEST,
            Self::Failed(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub(crate) fn message(&self) -> String {
        match self {
            Self::Locked => WATCHING_ONLY.to_string(),
            Self::NotDriving => "no turn is running here that this server can abort".to_string(),
            Self::Busy(message)
            | Self::NotFound(message)
            | Self::Invalid(message)
            | Self::Failed(message) => message.clone(),
        }
    }
}

/// A turn this process is running — or is about to. The entry is made
/// the moment a request *decides* to start a turn, before the slow
/// `plan()`, because that decision is the thing a second request racing
/// it has to be able to see: without it the loser reaches the writer
/// lease, finds it taken by this very server, and is told the session
/// belongs to another process.
#[derive(Debug)]
struct DrivenTurn {
    /// Where a steer goes. `None` while the entry is only a reservation,
    /// and again once an abort has closed the loop's ears: a message
    /// must never be accepted into a channel nobody will drain.
    steer: Option<SteerSender>,
    /// Stops the turn — including one that has not started yet, which
    /// then gets its token already cancelled and returns on its first
    /// check rather than running a turn nobody wants.
    cancel: CancellationToken,
    /// Aborted: the turn is unwinding and still holds the writer lease,
    /// so the session is still this server's, but it takes no messages.
    stopping: bool,
    /// Which turn this is. A finishing turn must not evict the entry of
    /// the one that replaced it: `run_turn` releases the writer lease
    /// when it returns, so a message arriving in the moment before its
    /// task finishes cleaning up legitimately starts the next turn on
    /// the same session, and the loser of that race is whoever removes
    /// a key it no longer owns.
    epoch: u64,
}

impl DrivenTurn {
    fn steerable(&self) -> bool {
        !self.stopping && self.steer.is_some()
    }

    /// What to tell a client whose message cannot be delivered right
    /// now. Both states clear themselves within a turn's setup or its
    /// unwind, so the sentence asks for the message again rather than
    /// declaring the session somebody else's.
    fn busy(&self) -> DriveError {
        DriveError::Busy(if self.stopping {
            "the turn here is stopping; send that again in a moment".into()
        } else {
            "a turn is already starting here; send that again in a moment".into()
        })
    }
}

/// The sessions this process is driving, by id.
type Registry = Arc<Mutex<HashMap<String, DrivenTurn>>>;

/// A session's long-lived machinery: the runtime every driven turn on
/// it reuses, and the consumer that delivers its background
/// completions. Built by the first turn, kept for as long as this
/// process serves the session — exactly the lifetime the TUI gives the
/// same objects — and torn down with the drive, never with a turn.
struct Engine {
    runtime: Arc<SessionRuntime>,
    /// Stops the consumer, and whatever wait it is in, at teardown.
    cancel: CancellationToken,
    consumer: tokio::task::JoinHandle<()>,
}

/// The kept runtimes, by session id.
type Engines = Arc<Mutex<HashMap<String, Engine>>>;

/// Same poisoning stance as [`lock`]: the engines are consulted on the
/// write path only, and a panic there must not wedge the next request.
fn lock_engines(engines: &Engines) -> MutexGuard<'_, HashMap<String, Engine>> {
    engines
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// How often a delivery waiting on the turn slot looks again. The slot
/// always clears — a turn ends, or its panic drops it — so this wait is
/// not backed off, only cancelled.
const SLOT_RETRY: Duration = Duration::from_millis(250);
/// First delay of the exponential backoff a delivery retries with.
const RETRY_BASE: Duration = Duration::from_millis(500);
/// Ceiling for any single backoff delay.
const RETRY_CEILING: Duration = Duration::from_secs(5);
/// `Requeue` outcomes retried before a routed notification is dropped.
const ROUTE_RETRY_LIMIT: usize = 8;
/// Lease attempts before a same-session delivery is dropped: another
/// process (a TUI, say) has held the session for minutes.
const LEASE_RETRY_LIMIT: u32 = 30;
/// How long teardown waits for a consumer mid-delivery after cancelling
/// its turn. A backstop, not the plan.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// Take the registry lock, ignoring poisoning. A panic on the write path
/// must not take the read routes down with it — `drives()` is on the
/// listing, which every page polls. What a panic can leave behind is one
/// stale entry, and the [`TurnSlot`] it was created with removes that on
/// its way out of the unwinding task.
fn lock(running: &Registry) -> MutexGuard<'_, HashMap<String, DrivenTurn>> {
    running
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The registry entry, for as long as a turn — or the request preparing
/// one — owns it. Dropping it removes the entry: on the error path out
/// of `message`, at the end of the turn's task, and on a panic in
/// either. A session cannot be left permanently `driven` by a turn that
/// is not running.
#[derive(Debug)]
struct TurnSlot {
    running: Registry,
    id: String,
    epoch: u64,
    cancel: CancellationToken,
}

impl Drop for TurnSlot {
    fn drop(&mut self) {
        let mut running = lock(&self.running);
        // Only this turn's own entry: by now a later turn may have taken
        // the session, and removing its entry would leave a running turn
        // that cannot be steered or stopped.
        if running
            .get(&self.id)
            .is_some_and(|turn| turn.epoch == self.epoch)
        {
            running.remove(&self.id);
        }
    }
}

/// What a message did with the registry: reached the running turn, or
/// claimed the session for a new one.
#[derive(Debug)]
enum Claim {
    Steered,
    Start(TurnSlot),
}

/// A turn that failed, for the streams watching that session.
///
/// The store has no shape for this. A provider failure is persisted as a
/// `Diagnostic` block that the wire projection deliberately drops, and a
/// failure before the loop (an unresolvable runtime, a store that will
/// not open) is not persisted at all — so without this the page shows a
/// turn that simply stopped, and the reason goes to the server's stderr
/// where nobody is looking.
#[derive(Debug, Clone)]
pub(crate) struct TurnFailure {
    pub(crate) session_id: String,
    pub(crate) message: String,
}

/// Failures buffered per subscriber. A stream that falls this far behind
/// its own session's failures has bigger problems than a missed frame.
const FAILURE_CAPACITY: usize = 64;

/// What a new session asks for. `cwd` and `model` are optional the same
/// way they are on the command line: absent means the server's own
/// directory and the configured default.
#[derive(Debug, Clone, Default)]
pub(crate) struct NewSession {
    pub(crate) prompt: String,
    pub(crate) cwd: Option<String>,
    pub(crate) model: Option<String>,
}

/// The runtime behind the write routes.
pub(crate) struct Drive {
    config: Arc<Config>,
    store: SessionStore,
    /// Injected in tests, where a scripted provider stands in for the
    /// network. `None` in every real run: the turn uses the resolver its
    /// own `RuntimePlan` built from configuration, which is also the one
    /// its subagents get.
    resolver: Option<Arc<dyn ProviderResolver>>,
    running: Registry,
    engines: Engines,
    epochs: Arc<AtomicU64>,
    failures: broadcast::Sender<TurnFailure>,
}

impl Drive {
    pub(crate) fn new(config: Config, store: SessionStore) -> Self {
        Self {
            config: Arc::new(config),
            store,
            resolver: None,
            running: Arc::new(Mutex::new(HashMap::new())),
            engines: Arc::new(Mutex::new(HashMap::new())),
            epochs: Arc::new(AtomicU64::new(0)),
            failures: broadcast::channel(FAILURE_CAPACITY).0,
        }
    }

    /// Every turn failure this process sees from here on. The SSE route
    /// subscribes per stream and keeps the frames for its own session.
    pub(crate) fn failures(&self) -> broadcast::Receiver<TurnFailure> {
        self.failures.subscribe()
    }

    /// Run turns against this resolver instead of the configured
    /// providers. Only the driven turn is redirected; subagents keep the
    /// runtime's own resolver, which is why this is a test seam and not
    /// a supported way to run.
    #[cfg(test)]
    pub(crate) fn with_resolver(mut self, resolver: Arc<dyn ProviderResolver>) -> Self {
        self.resolver = Some(resolver);
        self
    }

    /// Whether this process is running a turn on that session — the
    /// difference between an abort control and a dot. True from the
    /// moment a request claims the session, because from that moment on
    /// this server is what stands between it and another writer.
    pub(crate) fn drives(&self, id: &str) -> bool {
        lock(&self.running).contains_key(id)
    }

    /// Create a session and run its first turn.
    pub(crate) async fn create(&self, request: NewSession) -> Result<String, DriveError> {
        let prompt = request.prompt.trim().to_string();
        if prompt.is_empty() {
            return Err(DriveError::Invalid("a prompt is required".into()));
        }
        let cwd = match request
            .cwd
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
        {
            Some(cwd) => {
                let path = PathBuf::from(cwd);
                if !path.is_dir() {
                    return Err(DriveError::Invalid(format!("{cwd} is not a directory")));
                }
                path
            }
            None => std::env::current_dir()
                .map_err(|error| DriveError::Failed(format!("no working directory: {error}")))?,
        };
        let options = RuntimeOptions {
            model: request
                .model
                .map(|m| m.trim().to_string())
                .filter(|m| !m.is_empty()),
            resume: None,
            cwd,
            // Headless, exactly as `ilar exec` is: no question tool.
            questions: false,
            ..RuntimeOptions::default()
        };
        let runtime = self.adopt(self.plan(options).await?);
        let id = runtime.session_id.clone();
        // Nothing can be racing this one: the id was minted a moment ago
        // by the plan that created the session.
        let slot = self.reserve(&mut lock(&self.running), &id);
        self.spawn_turn(runtime, prompt, None, slot);
        Ok(id)
    }

    /// Send a message to a session: steer the turn this process is
    /// running, or start one.
    pub(crate) async fn message(&self, id: &str, text: &str) -> Result<Fate, DriveError> {
        let text = text.trim().to_string();
        if text.is_empty() {
            return Err(DriveError::Invalid("an empty message says nothing".into()));
        }
        let slot = match self.claim(id, &text)? {
            Claim::Steered => return Ok(Fate::Steering),
            Claim::Start(slot) => slot,
        };
        // Everything from here can fail, and every failure drops the
        // slot, which gives the session back.
        if let Some(runtime) = self.runtime_for(id) {
            // Driven here before: the session's runtime — spawner,
            // services, background children and all — is still alive,
            // and re-planning it would orphan them. Only the lease
            // pre-check stays per turn; it is what notices another
            // process having taken the session in between.
            let lease = self.acquire(id).await?;
            self.spawn_turn(runtime, text, Some(lease), slot);
            return Ok(Fate::Started);
        }
        let cwd = self.turn_cwd(id).await?;
        let lease = self.acquire(id).await?;
        let options = RuntimeOptions {
            resume: Some(id.to_string()),
            cwd,
            questions: false,
            ..RuntimeOptions::default()
        };
        let runtime = self.adopt(self.plan(options).await?);
        self.spawn_turn(runtime, text, Some(lease), slot);
        Ok(Fate::Started)
    }

    /// Cancel the turn this process is running on that session.
    pub(crate) fn abort(&self, id: &str) -> Result<Fate, DriveError> {
        let mut running = lock(&self.running);
        let Some(turn) = running.get_mut(id) else {
            return Err(DriveError::NotDriving);
        };
        turn.cancel.cancel();
        // The entry stays until the turn lets go of the writer lease —
        // this server is still the one holding the session — but it
        // stops taking messages: the loop checks its cancellation before
        // it drains steers, so a steer accepted now would be read by
        // nobody. Marking it is what makes the next message start a turn
        // rather than disappear into a loop that has already returned.
        turn.stopping = true;
        turn.steer = None;
        Ok(Fate::Aborted)
    }

    /// Deciding and claiming, in one critical section. The TUI's own
    /// steer-vs-start rule ([`decide::submit_target`]) makes the call,
    /// asked with what serve can see; `busy` is false because the server
    /// has no post-turn settling state to protect — the registry entry
    /// *is* the turn's lifetime. Two messages racing an idle session
    /// cannot both decide to start a turn, because the winner's
    /// reservation is in the map before the lock is released.
    fn claim(&self, id: &str, text: &str) -> Result<Claim, DriveError> {
        let mut running = lock(&self.running);
        let target = decide::submit_target(
            &LoopState {
                turn_running: running.contains_key(id),
                steerable: running.get(id).is_some_and(DrivenTurn::steerable),
                ..LoopState::default()
            },
            false,
        );
        match target {
            // The channel is unbounded and the turn drains it at every
            // step boundary.
            SubmitTarget::Steer => match running.get(id).and_then(|turn| turn.steer.as_ref()) {
                // Words only: the web client posts text, so there is
                // nothing to attach here the way the TUI attaches what
                // is pending on the prompt.
                Some(steer) if steer.send(text.into()).is_ok() => Ok(Claim::Steered),
                // The loop dropped its receiver between the decision and
                // the send: the turn is over but has not cleaned up, and
                // it still holds the writer lease. Starting one on top of
                // it would reach that lease and report another process.
                _ => Err(DriveError::Busy(
                    "the turn here is finishing; send that again in a moment".into(),
                )),
            },
            // A turn is here and cannot take a message: a reservation
            // whose runtime is still resolving, or one unwinding after an
            // abort. Serve has no queue to hold it in, and the client
            // keeps the draft.
            SubmitTarget::Queue => Err(running.get(id).map_or_else(
                || DriveError::Busy("a turn is already running here".into()),
                DrivenTurn::busy,
            )),
            SubmitTarget::StartTurn => Ok(Claim::Start(self.reserve(&mut running, id))),
        }
    }

    /// Claim the session for a turn about to be prepared. The entry goes
    /// in before the slow part, and the slot is what gives it back.
    fn reserve(&self, running: &mut HashMap<String, DrivenTurn>, id: &str) -> TurnSlot {
        reserve_locked(running, &self.running, &self.epochs, id)
    }

    /// Where a resumed turn runs: the directory the session recorded.
    /// That is where its tools edit, where its subagents look and where
    /// its project instructions come from — a turn resumed in the
    /// server's own directory would work on the wrong tree while the
    /// page names the right one. The server's directory is the fallback
    /// for a session that recorded none (and for one whose directory has
    /// since gone), never the default.
    ///
    /// The head read doubles as the existence check: a session that is
    /// not there is a 404 here, not a turn that fails five steps later.
    async fn turn_cwd(&self, id: &str) -> Result<PathBuf, DriveError> {
        let store = self.store.clone();
        let owned = id.to_string();
        let head = tokio::task::spawn_blocking(move || store.head(&owned))
            .await
            .map_err(|error| DriveError::Failed(format!("reader task failed: {error}")))?
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::NotFound => {
                    DriveError::NotFound(format!("session not found: {id}"))
                }
                std::io::ErrorKind::InvalidInput => DriveError::Invalid(error.to_string()),
                _ => DriveError::Failed(error.to_string()),
            })?;
        match head.meta.cwd.filter(|cwd| cwd.is_dir()) {
            Some(cwd) => Ok(cwd),
            None => std::env::current_dir()
                .map_err(|error| DriveError::Failed(format!("no working directory: {error}"))),
        }
    }

    /// Take the writer lease, or say who has it. Held from here until
    /// the turn task hands it to `run_turn`.
    async fn acquire(&self, id: &str) -> Result<SessionWriter, DriveError> {
        acquire_writer(self.store.clone(), id.to_string())
            .await
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::WouldBlock => DriveError::Locked,
                std::io::ErrorKind::NotFound => {
                    DriveError::NotFound(format!("session not found: {id}"))
                }
                std::io::ErrorKind::InvalidInput => DriveError::Invalid(error.to_string()),
                _ => DriveError::Failed(error.to_string()),
            })
    }

    /// Resolve the same runtime a TUI launch would. Blocking: it reads
    /// the config tree, the agent and skill definitions, and (for a
    /// resume) the session log.
    async fn plan(&self, options: RuntimeOptions) -> Result<SessionRuntime, DriveError> {
        let config = self.config.clone();
        tokio::task::spawn_blocking(move || {
            RuntimePlan::resolve(&config, &options).and_then(|plan| plan.start(&config))
        })
        .await
        .map_err(|error| DriveError::Failed(format!("runtime task failed: {error}")))?
        .map_err(|error| DriveError::Failed(format!("{error:#}")))
    }

    /// Run the turn the slot was claimed for. The session has been
    /// "driven" since that claim; what the spawned task adds first is
    /// the steer channel, so the caller's response tells the truth about
    /// what a follow-up message will do.
    fn spawn_turn(
        &self,
        runtime: Arc<SessionRuntime>,
        prompt: String,
        lease: Option<SessionWriter>,
        slot: TurnSlot,
    ) {
        let ctx = self.turn_context(&runtime);
        tokio::spawn(run_driven_turn(ctx, prompt, lease, slot));
    }

    /// Keep the freshly planned runtime for the session's lifetime under
    /// this drive, and start the one consumer of its background
    /// notifications. Keyed by session id; nothing short of
    /// [`Self::shutdown`] invalidates an entry — `run_turn` re-reads the
    /// effective model from the session log every turn, so a model
    /// change never stales the cache, and configuration or agent
    /// definitions edited on disk are picked up no later than the TUI
    /// picks them up, which also builds once per session.
    fn adopt(&self, runtime: SessionRuntime) -> Arc<SessionRuntime> {
        let runtime = Arc::new(runtime);
        let id = runtime.session_id.clone();
        let mut engines = lock_engines(&self.engines);
        if let Some(engine) = engines.get(&id) {
            // Cannot happen on today's paths — the turn slot serializes
            // a session's turns, and `message` checks the cache before
            // planning — but if a plan ever races an engine, the
            // incumbent keeps the session: it holds the one notification
            // subscription, and a second one would read nothing.
            return engine.runtime.clone();
        }
        let cancel = CancellationToken::new();
        // The channel's single subscription, taken exactly once, here.
        let notifications = runtime.spawner.subscribe();
        let consumer = Consumer {
            session_id: id.clone(),
            turn: self.turn_context(&runtime),
            epochs: self.epochs.clone(),
            cancel: cancel.clone(),
            // The same directory the spawner records into
            // (`RuntimePlan` wires `with_outbox_dir` from this exact
            // path): adoption reads back what an earlier process
            // published for this tree and never delivered.
            outbox_dir: self.config.state_dir().join("outbox"),
        };
        let handle = tokio::spawn(consumer.run(notifications));
        engines.insert(
            id,
            Engine {
                runtime: runtime.clone(),
                cancel,
                consumer: handle,
            },
        );
        runtime
    }

    /// The kept runtime for a session this process drove before.
    fn runtime_for(&self, id: &str) -> Option<Arc<SessionRuntime>> {
        lock_engines(&self.engines)
            .get(id)
            .map(|engine| engine.runtime.clone())
    }

    fn turn_context(&self, runtime: &Arc<SessionRuntime>) -> TurnContext {
        TurnContext {
            runtime: runtime.clone(),
            resolver: self
                .resolver
                .clone()
                .unwrap_or_else(|| runtime.resolver.clone()),
            running: self.running.clone(),
            failures: self.failures.clone(),
        }
    }

    /// Tear down every session this process drove: cancel the running
    /// turns, stop the consumers, abort the background children with the
    /// spawner's own grace, and kill the services. The teardown that was
    /// once (wrongly) each turn's, now at the only boundary where
    /// nothing survives anyway — process exit.
    pub(crate) async fn shutdown(&self) {
        let engines = lock_engines(&self.engines)
            .drain()
            .map(|(_, engine)| engine)
            .collect::<Vec<_>>();
        // Consumers first: a consumer between taking a slot and
        // starting its turn would otherwise launch work the running
        // sweep below has already missed.
        for engine in &engines {
            engine.cancel.cancel();
        }
        {
            // A turn still running stops the way an abort stops it; its
            // slot cleans the registry entry up on the way out.
            let mut running = lock(&self.running);
            for turn in running.values_mut() {
                turn.cancel.cancel();
                turn.stopping = true;
                turn.steer = None;
            }
        }
        let mut background = 0;
        for engine in engines {
            // A consumer mid-delivery finishes its now-cancelled turn
            // first; the grace is a backstop, not the plan.
            let _ = tokio::time::timeout(SHUTDOWN_GRACE, engine.consumer).await;
            background += engine.runtime.spawner.running_background();
            engine.runtime.spawner.shutdown().await;
            engine.runtime.services.stop_all();
        }
        if background > 0 {
            // The same sentence `ilar exec` says at its exit.
            eprintln!("serve: {background} background task(s) cancelled at exit");
        }
    }
}

/// What one driven turn needs of the drive, detached from it so the
/// notification consumer can run turns without holding the `Drive`.
#[derive(Clone)]
struct TurnContext {
    runtime: Arc<SessionRuntime>,
    resolver: Arc<dyn ProviderResolver>,
    running: Registry,
    failures: broadcast::Sender<TurnFailure>,
}

/// One driven turn, start to cleanup — the web message's and the
/// notification delivery's shared path, so SSE frames, the failure
/// broadcast and the slot discipline cannot drift apart.
async fn run_driven_turn(
    ctx: TurnContext,
    prompt: String,
    lease: Option<SessionWriter>,
    slot: TurnSlot,
) {
    let (steer, steer_rx) = steer_channel();
    let cancel = slot.cancel.clone();
    let id = ctx.runtime.session_id.clone();
    {
        let mut running = lock(&ctx.running);
        // An abort that landed while the runtime resolved leaves the
        // entry `stopping`; the cancelled token below carries that, and
        // the ears stay closed.
        if let Some(turn) = running.get_mut(&id)
            && turn.epoch == slot.epoch
            && !turn.stopping
        {
            turn.steer = Some(steer);
        }
    }
    // The slot rides with this future: whether the turn returns, fails
    // or panics, its exit is what stops the session being driven.
    // The lease was the caller's proof that nobody else owns the
    // session; `run_turn` takes its own, so it is released here and
    // nowhere earlier.
    drop(lease);
    let (events, mut receiver) = loop_event_channel(LOOP_EVENT_CAPACITY);
    // The loop publishes with backpressure, so somebody has to read.
    // Nothing here needs the events — the store and the live scratch are
    // the record, and the SSE reads those — but an unread channel would
    // stall the turn at 64 events.
    let drain = tokio::spawn(async move { while receiver.recv().await.is_some() {} });

    let outcome = ilar::agent::run_turn(
        ctx.resolver.as_ref(),
        &ctx.runtime.registry,
        &ctx.runtime.store,
        &id,
        &prompt,
        &[],
        Some(&ctx.runtime.system_prompt),
        ctx.runtime.loop_config.clone(),
        events,
        cancel,
        ctx.runtime.tool_ctx.clone(),
        Some(steer_rx),
    )
    .await;
    drain.abort();

    // Deliberately no spawner shutdown and no service stop here: both
    // are the session's, not the turn's, torn down in `Drive::shutdown`.
    // A background child started by this turn keeps running, and the
    // session's `Consumer` is what its completion reaches.

    // The session stops being driven here, and not before the lease
    // `run_turn` returned is already released.
    drop(slot);
    if let Err(error) = outcome {
        let message = format!("the turn failed: {error:#}");
        eprintln!("serve: turn on {id}: {message}");
        // Whoever is watching this session hears it. Nobody watching is
        // not an error: the send fails when there is no subscriber,
        // which is the common case.
        let _ = ctx.failures.send(TurnFailure {
            session_id: id,
            message,
        });
    }
}

/// Claim the session for a turn about to be prepared, under the
/// caller's lock on the registry.
fn reserve_locked(
    map: &mut HashMap<String, DrivenTurn>,
    running: &Registry,
    epochs: &AtomicU64,
    id: &str,
) -> TurnSlot {
    let epoch = epochs.fetch_add(1, Ordering::Relaxed);
    let cancel = CancellationToken::new();
    map.insert(
        id.to_string(),
        DrivenTurn {
            steer: None,
            cancel: cancel.clone(),
            stopping: false,
            epoch,
        },
    );
    TurnSlot {
        running: running.clone(),
        id: id.to_string(),
        epoch,
        cancel,
    }
}

/// The consumer's claim: a notification turn starts only on an idle
/// session — never a steer into a running one, which is also what the
/// TUI does with a completion that lands mid-turn — so an occupied
/// entry is "come back later", not a decision to make.
fn try_reserve(running: &Registry, epochs: &AtomicU64, id: &str) -> Option<TurnSlot> {
    let mut map = lock(running);
    if map.contains_key(id) {
        return None;
    }
    Some(reserve_locked(&mut map, running, epochs, id))
}

/// Take the writer lease off the async thread.
async fn acquire_writer(store: SessionStore, id: String) -> std::io::Result<SessionWriter> {
    tokio::task::spawn_blocking(move || store.acquire_writer(&id))
        .await
        .map_err(|error| std::io::Error::other(format!("writer task failed: {error}")))?
}

/// The single reader of one session's background-completion channel,
/// and the dispatcher of its deliveries. A delivery is a whole turn —
/// a follow-up on the driven session, or a routed resume of a child —
/// so two must never interleave on one *target* session; but a delivery
/// that is merely waiting (a busy child's claim, a lease another
/// process holds) must not hold every other target's mail behind it.
/// Hence one worker per target session, each strictly serial, all torn
/// down with the engine.
struct Consumer {
    session_id: String,
    turn: TurnContext,
    epochs: Arc<AtomicU64>,
    /// The engine's token: teardown, not any turn's cancellation.
    cancel: CancellationToken,
    /// Where the spawner's durable copies of published notifications
    /// live; read once, at adoption.
    outbox_dir: PathBuf,
}

/// Hand a parcel to its target's worker, creating the worker on first
/// mail. Keyed by the notification's parent session — the session a
/// delivery occupies — so serialization per key is exactly "no two
/// concurrent delivery turns on one session" and nothing broader.
fn dispatch(
    consumer: &Arc<Consumer>,
    queues: &mut HashMap<String, tokio::sync::mpsc::UnboundedSender<Parcel>>,
    workers: &mut Vec<tokio::task::JoinHandle<()>>,
    redispatch: &tokio::sync::mpsc::UnboundedSender<Parcel>,
    parcel: Parcel,
) {
    let queue = queues
        .entry(parcel.notification().parent_session_id.clone())
        .or_insert_with(|| {
            let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
            workers.push(tokio::spawn(
                consumer.clone().work(receiver, redispatch.clone()),
            ));
            sender
        });
    // A worker exits only at teardown, when losing this mail is the
    // point; the outbox keeps the durable copy either way.
    let _ = queue.send(parcel);
}

impl Consumer {
    async fn run(self, mut notifications: tokio::sync::mpsc::Receiver<Notification>) {
        let consumer = Arc::new(self);
        // Where a propagated notification re-enters: the climb changed
        // its target session, so it changes queues too.
        let (redispatch, mut propagated) = tokio::sync::mpsc::unbounded_channel::<Parcel>();
        let mut queues = HashMap::new();
        let mut workers = Vec::new();
        // Adoption: everything some process — this one or an earlier
        // life — published for this tree and never delivered. `pending`
        // proves delivery against the parents' own logs, so anything
        // that already landed is not here to deliver twice.
        for notification in consumer.recovered().await {
            dispatch(
                &consumer,
                &mut queues,
                &mut workers,
                &redispatch,
                Parcel::fresh(notification),
            );
        }
        loop {
            let parcel = tokio::select! {
                () = consumer.cancel.cancelled() => break,
                Some(parcel) = propagated.recv() => parcel,
                notification = notifications.recv() => match notification {
                    Some(notification) => Parcel::fresh(notification),
                    // The spawner is gone; so is anything to deliver.
                    None => break,
                },
            };
            dispatch(&consumer, &mut queues, &mut workers, &redispatch, parcel);
        }
        // Closing the queues lets an undisturbed worker drain and end;
        // on teardown the token stops each mid-wait instead, and the
        // shutdown grace bounds this await either way.
        drop(queues);
        drop(redispatch);
        for worker in workers {
            let _ = worker.await;
        }
    }

    /// One target session's strictly serial delivery loop.
    async fn work(
        self: Arc<Self>,
        mut queue: tokio::sync::mpsc::UnboundedReceiver<Parcel>,
        redispatch: tokio::sync::mpsc::UnboundedSender<Parcel>,
    ) {
        loop {
            let parcel = tokio::select! {
                () = self.cancel.cancelled() => return,
                parcel = queue.recv() => match parcel {
                    Some(parcel) => parcel,
                    None => return,
                },
            };
            self.deliver(parcel, &redispatch).await;
        }
    }

    /// One completion, to whichever session it belongs: the driven
    /// session's own becomes a follow-up turn here; a child's child's
    /// is routed downward, and what `Propagate` hands back goes to the
    /// dispatcher to climb one hop at a time until it is ours or the
    /// hop budget says the tree is deeper than the spawner allows.
    async fn deliver(&self, parcel: Parcel, redispatch: &tokio::sync::mpsc::UnboundedSender<Parcel>) {
        if self.cancel.is_cancelled() {
            return;
        }
        if parcel.notification().parent_session_id == self.session_id {
            self.follow_up(parcel.into_notification()).await;
            return;
        }
        let climbed = parcel.notification().clone();
        if let Some(propagated) = self.route(climbed).await {
            // `ilar::delivery::Parcel` counts the climb: `None` means
            // the hop it just took was its last, which only a parent
            // chain that loops ever reaches.
            let next = match parcel.climbing(propagated) {
                Ok(next) => next,
                Err(stranded) => {
                    eprintln!(
                        "serve: dropping a task completion for {} after {} hops",
                        stranded.parent_session_id,
                        ilar::delivery::PROPAGATION_HOPS
                    );
                    return;
                }
            };
            let _ = redispatch.send(next);
        }
    }

    /// Whether the notification's text already sits in a UserMessage of
    /// the target's log. `ilar::delivery` owns the definition — this
    /// only supplies the log and a thread to read it on, since the read
    /// blocks.
    async fn already_delivered(&self, notification: &Notification) -> bool {
        let store = self.turn.runtime.store.clone();
        let id = self.session_id.clone();
        let text = notification.text.clone();
        tokio::task::spawn_blocking(move || {
            let Ok(session) = store.load(&id) else {
                return false;
            };
            ilar::delivery::is_delivered(&session, &text)
        })
        .await
        .unwrap_or(false)
    }

    /// What the outbox holds for this session's tree. Blocking work —
    /// directory scans and log reads — so it runs off the async thread.
    async fn recovered(&self) -> Vec<Notification> {
        let store = self.turn.runtime.store.clone();
        let dir = self.outbox_dir.clone();
        let root = self.session_id.clone();
        let recovered =
            tokio::task::spawn_blocking(move || ilar::outbox::pending(&store, &dir, &root))
                .await
                .unwrap_or_default();
        if !recovered.is_empty() {
            eprintln!(
                "serve: requeueing {} recorded task result(s) for {}",
                recovered.len(),
                self.session_id
            );
        }
        recovered
    }

    /// Deliver a completion to the driven session as a follow-up turn,
    /// through the same slot-and-lease path a web message takes. The
    /// prompt is the notification's own text — the `<task-notification>`
    /// envelope the parent loop unwraps, exactly what the TUI feeds its
    /// notification turns.
    async fn follow_up(&self, notification: Notification) {
        // The same guard route_notification takes: an adopted outbox
        // entry can race another process delivering the same
        // completion, and the session's own log is the truth. Checked
        // once up front and again after every backoff, since the race
        // is exactly "someone else delivered while we waited".
        let mut lease_attempts = 0_u32;
        let mut delay = RETRY_BASE;
        loop {
            if self.already_delivered(&notification).await {
                return;
            }
            let Some(slot) = try_reserve(&self.turn.running, &self.epochs, &self.session_id)
            else {
                // A turn is running or starting; its slot always
                // clears, so this wait is patient, not bounded.
                if !self.pause(SLOT_RETRY).await {
                    return;
                }
                continue;
            };
            // The lease pre-check a web message performs; `run_turn`
            // takes its own. Locked means another *process* opened the
            // session between turns — back off without holding the
            // slot, so a web message meanwhile is answered honestly
            // instead of being told this server is starting a turn.
            match acquire_writer(self.turn.runtime.store.clone(), self.session_id.clone()).await {
                Ok(lease) => {
                    run_driven_turn(self.turn.clone(), notification.text, Some(lease), slot).await;
                    return;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    drop(slot);
                    lease_attempts += 1;
                    if lease_attempts > LEASE_RETRY_LIMIT {
                        // Dropped from this queue only: the outbox
                        // still holds the durable copy, and the next
                        // adoption of this tree requeues it.
                        eprintln!(
                            "serve: dropping a task completion for {}: the session is held by another process",
                            self.session_id
                        );
                        return;
                    }
                    if !self.pause(delay).await {
                        return;
                    }
                    delay = (delay * 2).min(RETRY_CEILING);
                }
                Err(error) => {
                    eprintln!(
                        "serve: dropping a task completion for {}: {error}",
                        self.session_id
                    );
                    return;
                }
            }
        }
    }

    /// Route a completion that belongs to a deeper session. `Propagate`
    /// comes back to the caller; `Requeue` is retried with backoff and
    /// then dropped from this queue — durability past that is the
    /// outbox, whose copy the next adoption of this tree requeues.
    async fn route(&self, mut notification: Notification) -> Option<Notification> {
        let mut delay = RETRY_BASE;
        for _ in 0..ROUTE_RETRY_LIMIT {
            match self
                .turn
                .runtime
                .spawner
                .route_notification(notification, self.cancel.child_token())
                .await
            {
                Ok(RouteOutcome::Complete) => return None,
                Ok(RouteOutcome::Propagate(up)) => return Some(up),
                Ok(RouteOutcome::Requeue(again)) => {
                    notification = again;
                    if !self.pause(delay).await {
                        return None;
                    }
                    delay = (delay * 2).min(RETRY_CEILING);
                }
                Err(error) => {
                    eprintln!("serve: routing a task completion failed: {error:#}");
                    return None;
                }
            }
        }
        eprintln!(
            "serve: dropping a task completion for {} after {ROUTE_RETRY_LIMIT} attempts",
            notification.parent_session_id
        );
        None
    }

    /// Sleep, unless torn down first; `false` says stop delivering.
    async fn pause(&self, delay: Duration) -> bool {
        tokio::select! {
            () = self.cancel.cancelled() => false,
            () = tokio::time::sleep(delay) => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ilar::provider::{MockProvider, ProviderEvent, StopReason};
    use ilar::session::Usage;
    use serde_json::{Value, json};
    use std::time::Duration;

    /// A server on an ephemeral port over a temp store, driving turns
    /// against a scripted provider. The whole router, the real watcher
    /// and the real session store — only the network is a stand-in.
    struct Harness {
        base: String,
        store: SessionStore,
        provider: MockProvider,
        client: reqwest::Client,
        _dir: tempfile::TempDir,
    }

    /// The configuration a driven turn resolves: a custom endpoint, so
    /// `RuntimePlan::start` finds a provider for the default model
    /// without a key in the environment. Nothing ever calls it — the
    /// injected `MockProvider` answers instead — so the URL is a dead
    /// port on purpose.
    const CONFIG: &str = r#"
[general]
model = "custom/mock"

[models.mock]
base_url = "http://127.0.0.1:9/v1"
context = 200000
"#;

    impl Harness {
        async fn start(turns: Vec<Vec<ProviderEvent>>) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let state = dir.path().join("state");
            let config_dir = dir.path().join("config");
            std::fs::create_dir_all(&state).unwrap();
            std::fs::create_dir_all(&config_dir).unwrap();
            std::fs::write(config_dir.join("ilar.toml"), CONFIG).unwrap();

            let config = ilar::config::Loader::no_env()
                .config_dir(config_dir)
                .state_dir(state.clone())
                .resolve()
                .unwrap();
            let root = state.join("sessions");
            std::fs::create_dir_all(&root).unwrap();
            let store = SessionStore::new(root.clone());

            let watcher = super::super::watch::Watcher::new(
                root,
                super::super::watch::WatchConfig::with_poll_ms(25),
            );
            watcher.refresh();
            watcher.spawn_poller();
            let provider = MockProvider::repeating(turns);
            let drive = Drive::new(config, store.clone())
                .with_resolver(Arc::new(provider.clone()) as Arc<dyn ProviderResolver>);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let state = super::super::http::ServeState {
                watcher,
                token: None,
                drive: Arc::new(drive),
                bind: address,
            };
            tokio::spawn(async move {
                let _ = axum::serve(listener, super::super::http::router(state)).await;
            });
            Self {
                base: format!("http://{address}"),
                store,
                provider,
                client: reqwest::Client::new(),
                _dir: dir,
            }
        }

        async fn post(&self, path: &str, body: Value) -> (reqwest::StatusCode, Value) {
            let response = self
                .client
                .post(format!("{}{path}", self.base))
                .json(&body)
                .send()
                .await
                .unwrap();
            let status = response.status();
            (status, response.json().await.unwrap_or(Value::Null))
        }

        async fn json(&self, path: &str) -> Value {
            self.client
                .get(format!("{}{path}", self.base))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap()
        }

        /// The transcript, once it satisfies `ready` — a driven turn is
        /// asynchronous by design, so every assertion about it waits.
        async fn transcript_once(&self, id: &str, ready: impl Fn(&Value) -> bool) -> Value {
            for _ in 0..200 {
                let page = self.json(&format!("/api/sessions/{id}")).await;
                if ready(&page) {
                    return page;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            panic!("the transcript never arrived");
        }

        /// Wait until the turn is running a tool, which is the point at
        /// which it has certainly taken a turn off the scripted
        /// provider.
        async fn until_working(&self, id: &str) {
            for _ in 0..400 {
                let listing = self.json("/api/sessions").await;
                let row = listing["sessions"]
                    .as_array()
                    .and_then(|rows| rows.iter().find(|row| row["id"] == json!(id)))
                    .cloned()
                    .unwrap_or(Value::Null);
                if row["activity"].is_string() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            panic!("the turn never started a tool");
        }

        /// Wait until this server has let go of the session, so the next
        /// message is a new turn rather than a steer.
        async fn until_idle(&self, id: &str) {
            for _ in 0..200 {
                let page = self.json(&format!("/api/sessions/{id}")).await;
                if page["session"]["driven"] == json!(false) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            panic!("the session was driven forever");
        }
    }

    /// A `Drive` with no server around it, for the decisions that happen
    /// entirely inside the registry.
    fn drive_only() -> (Drive, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("ilar.toml"), CONFIG).unwrap();
        let config = ilar::config::Loader::no_env()
            .config_dir(config_dir)
            .state_dir(state.clone())
            .resolve()
            .unwrap();
        let root = state.join("sessions");
        std::fs::create_dir_all(&root).unwrap();
        let store = SessionStore::new(root);
        (Drive::new(config, store), dir)
    }

    fn answer(text: &str) -> Vec<ProviderEvent> {
        vec![
            ProviderEvent::TextDelta(text.into()),
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            },
        ]
    }

    /// A bash call, which is how a test makes a turn take long enough to
    /// send it something mid-flight.
    fn sleep_call(id: &str, seconds: &str) -> Vec<ProviderEvent> {
        vec![
            ProviderEvent::ToolCallStarted {
                id: id.into(),
                name: "bash".into(),
                item_id: None,
            },
            ProviderEvent::ToolCallCompleted {
                id: id.into(),
                name: "bash".into(),
                input: json!({"command": format!("sleep {seconds}")}),
            },
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::ToolUse,
                usage: Usage::default(),
            },
        ]
    }

    fn assistant_text(page: &Value) -> String {
        page["events"]
            .as_array()
            .map(|events| {
                events
                    .iter()
                    .filter(|event| event["type"] == "assistant_message")
                    .flat_map(|event| event["content"].as_array().cloned().unwrap_or_default())
                    .filter(|block| block["type"] == "text")
                    .map(|block| block["text"].as_str().unwrap_or_default().to_string())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default()
    }

    /// The whole write path in one: the page posts a prompt, the server
    /// creates the session, runs the turn headless, and the answer is in
    /// the store the readers already tail.
    #[tokio::test]
    async fn a_new_session_is_created_and_its_first_turn_runs() {
        let harness = Harness::start(vec![answer("hello from the mock")]).await;

        let (status, body) = harness
            .post("/api/sessions", json!({"prompt": "say hello"}))
            .await;
        assert_eq!(status, 200, "{body}");
        let id = body["id"].as_str().expect("an id comes back").to_string();

        let page = harness
            .transcript_once(&id, |page| !assistant_text(page).is_empty())
            .await;
        assert_eq!(assistant_text(&page), "hello from the mock");
        // The prompt is a user message on the session, not a parameter
        // that vanished into the loop.
        let text = serde_json::to_string(&page["events"]).unwrap();
        assert!(text.contains("say hello"), "{text}");
    }

    /// An idle session takes a message as a new turn, and the events it
    /// produces reach a client over the SSE stream that already existed
    /// — the point of driving turns through the store rather than beside
    /// it.
    #[tokio::test]
    async fn a_message_to_an_idle_session_runs_a_turn_that_streams() {
        let harness = Harness::start(vec![answer("first"), answer("second")]).await;
        let (status, body) = harness
            .post("/api/sessions", json!({"prompt": "one"}))
            .await;
        assert_eq!(status, 200, "{body}");
        let id = body["id"].as_str().unwrap().to_string();
        harness
            .transcript_once(&id, |page| assistant_text(page).contains("first"))
            .await;
        // The first turn has to be finished before the second is a
        // "message to an idle session" rather than a steer.
        for _ in 0..200 {
            let listing = harness.json("/api/sessions").await;
            let row = listing["sessions"][0].clone();
            if row["driven"] == json!(false) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let mut stream = harness
            .client
            .get(format!("{}/api/sessions/{id}/events", harness.base))
            .send()
            .await
            .unwrap();

        let (status, body) = harness
            .post(
                &format!("/api/sessions/{id}/message"),
                json!({"text": "two"}),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["fate"], "started");

        // The stream carries the turn: its user message, then its answer.
        let mut seen = String::new();
        while seen.find("second").is_none() {
            let chunk = tokio::time::timeout(Duration::from_secs(10), stream.chunk())
                .await
                .expect("the stream said something")
                .unwrap()
                .expect("the stream stayed open");
            seen.push_str(&String::from_utf8_lossy(&chunk));
        }
        assert!(seen.contains("\"two\""), "the user message streams: {seen}");
        assert!(seen.contains("event: append"), "{seen}");
    }

    /// A message sent while a turn runs is a steer, not a second turn:
    /// it reaches the model at the next step, which is exactly what the
    /// TUI's decide layer says and what this asserts on the wire the
    /// provider saw.
    #[tokio::test]
    async fn a_message_during_a_running_turn_steers_it() {
        let harness = Harness::start(vec![
            sleep_call("call-1", "1"),
            answer("done after the steer"),
        ])
        .await;
        let (_, body) = harness
            .post("/api/sessions", json!({"prompt": "start working"}))
            .await;
        let id = body["id"].as_str().unwrap().to_string();

        // Wait for the turn to be in flight, then talk over it.
        let mut steered = None;
        for _ in 0..200 {
            let (status, body) = harness
                .post(
                    &format!("/api/sessions/{id}/message"),
                    json!({"text": "actually, look at the other file"}),
                )
                .await;
            if body["fate"] == "steering" {
                steered = Some(status);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(steered, Some(reqwest::StatusCode::OK), "never steered");

        harness
            .transcript_once(&id, |page| assistant_text(page).contains("done after"))
            .await;
        let requests = harness.provider.requests();
        assert!(requests.len() >= 2, "{} provider calls", requests.len());
        let wire = format!("{:?}", requests.last().unwrap());
        assert!(
            wire.contains("actually, look at the other file"),
            "the steer reached the next step: {wire}"
        );
    }

    /// Abort stops the turn this process is running, and says so; a
    /// session it is not driving has nothing to stop.
    #[tokio::test]
    async fn abort_cancels_a_driven_turn_and_refuses_when_there_is_none() {
        let harness = Harness::start(vec![sleep_call("call-1", "30"), answer("unreachable")]).await;
        let (_, body) = harness
            .post("/api/sessions", json!({"prompt": "start a long tool"}))
            .await;
        let id = body["id"].as_str().unwrap().to_string();

        let mut aborted = None;
        for _ in 0..200 {
            let (status, body) = harness
                .post(&format!("/api/sessions/{id}/abort"), json!({}))
                .await;
            if status == 200 {
                aborted = Some(body["fate"].as_str().unwrap_or_default().to_string());
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(aborted.as_deref(), Some("aborted"));

        // The turn lets go: the session stops being driven long before
        // the 30-second tool would have finished.
        for _ in 0..100 {
            let (status, _) = harness
                .post(&format!("/api/sessions/{id}/abort"), json!({}))
                .await;
            if status == 404 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the aborted turn never let go of the session");
    }

    /// The lease is the whole story: a session another process is
    /// writing cannot be driven here, and the refusal is the sentence
    /// the page shows.
    #[tokio::test]
    async fn a_session_held_by_another_writer_is_refused() {
        let harness = Harness::start(vec![answer("never")]).await;
        let (_, body) = harness
            .post("/api/sessions", json!({"prompt": "first"}))
            .await;
        let id = body["id"].as_str().unwrap().to_string();
        harness
            .transcript_once(&id, |page| !assistant_text(page).is_empty())
            .await;
        // Wait out the driven turn, then hold the lease from outside.
        for _ in 0..200 {
            if harness
                .post(&format!("/api/sessions/{id}/abort"), json!({}))
                .await
                .0
                == 404
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let held = loop {
            match harness.store.acquire_writer(&id) {
                Ok(writer) => break writer,
                Err(_) => tokio::time::sleep(Duration::from_millis(25)).await,
            }
        };

        let (status, body) = harness
            .post(
                &format!("/api/sessions/{id}/message"),
                json!({"text": "hi"}),
            )
            .await;
        assert_eq!(status, 409, "{body}");
        assert_eq!(body["error"], WATCHING_ONLY);
        drop(held);
    }

    /// The registry is what "driven" means, and the listing says it, so
    /// a reloaded page knows which sessions it can abort.
    #[tokio::test]
    async fn the_listing_says_which_sessions_this_server_drives() {
        let harness = Harness::start(vec![sleep_call("call-1", "5"), answer("done")]).await;
        let (_, body) = harness
            .post("/api/sessions", json!({"prompt": "work"}))
            .await;
        let id = body["id"].as_str().unwrap().to_string();
        for _ in 0..200 {
            let listing = harness.json("/api/sessions").await;
            if listing["sessions"][0]["driven"] == json!(true) {
                assert_eq!(listing["sessions"][0]["id"], json!(id));
                let _ = harness
                    .post(&format!("/api/sessions/{id}/abort"), json!({}))
                    .await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("the listing never reported the driven session");
    }

    /// A prompt with nothing in it is a client error, not an empty
    /// session left behind.
    #[tokio::test]
    async fn an_empty_prompt_creates_nothing() {
        let harness = Harness::start(vec![answer("never")]).await;
        let (status, body) = harness
            .post("/api/sessions", json!({"prompt": "   "}))
            .await;
        assert_eq!(status, 400, "{body}");
        let listing = harness.json("/api/sessions").await;
        assert_eq!(listing["sessions"], json!([]));
    }

    /// A resumed turn belongs in the session's own tree. The proof is
    /// the one thing a directory cannot lie about: what the turn's own
    /// shell reports as its working directory.
    #[tokio::test]
    async fn a_resumed_turn_runs_where_the_session_says_it_does() {
        let project = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(project.path()).unwrap();
        let harness = Harness::start(vec![
            answer("ready"),
            vec![
                ProviderEvent::ToolCallStarted {
                    id: "pwd-1".into(),
                    name: "bash".into(),
                    item_id: None,
                },
                ProviderEvent::ToolCallCompleted {
                    id: "pwd-1".into(),
                    name: "bash".into(),
                    input: json!({"command": "pwd"}),
                },
                ProviderEvent::TurnComplete {
                    stop_reason: StopReason::ToolUse,
                    usage: Usage::default(),
                },
            ],
            answer("that is where I am"),
        ])
        .await;

        let (status, body) = harness
            .post(
                "/api/sessions",
                json!({"prompt": "one", "cwd": cwd.display().to_string()}),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        let id = body["id"].as_str().unwrap().to_string();
        harness
            .transcript_once(&id, |page| assistant_text(page).contains("ready"))
            .await;
        harness.until_idle(&id).await;

        let (status, body) = harness
            .post(
                &format!("/api/sessions/{id}/message"),
                json!({"text": "where are you?"}),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["fate"], "started");

        let page = harness
            .transcript_once(&id, |page| {
                page["events"]
                    .as_array()
                    .is_some_and(|events| events.iter().any(|e| e["type"] == "tool_result"))
            })
            .await;
        let result = page["events"]
            .as_array()
            .unwrap()
            .iter()
            .find(|event| event["type"] == "tool_result")
            .expect("the pwd result")
            .to_string();
        assert!(
            result.contains(cwd.to_str().unwrap()),
            "the turn ran in the session's directory, not the server's: {result}"
        );
    }

    /// A turn that fails has to reach the page. The log has no line that
    /// says so, and the server's stderr is not where anyone is looking.
    #[tokio::test]
    async fn a_failed_turn_reaches_the_stream_as_an_error_frame() {
        let harness = Harness::start(vec![
            answer("first"),
            vec![ProviderEvent::Error("the provider fell over".into())],
        ])
        .await;
        let (_, body) = harness
            .post("/api/sessions", json!({"prompt": "one"}))
            .await;
        let id = body["id"].as_str().unwrap().to_string();
        harness
            .transcript_once(&id, |page| assistant_text(page).contains("first"))
            .await;
        harness.until_idle(&id).await;

        let mut stream = harness
            .client
            .get(format!("{}/api/sessions/{id}/events", harness.base))
            .send()
            .await
            .unwrap();
        let (status, body) = harness
            .post(
                &format!("/api/sessions/{id}/message"),
                json!({"text": "two"}),
            )
            .await;
        assert_eq!(status, 200, "{body}");

        let mut seen = String::new();
        while !seen.contains("event: error") {
            let chunk = tokio::time::timeout(Duration::from_secs(10), stream.chunk())
                .await
                .expect("the failure reached the stream")
                .unwrap()
                .expect("the stream stayed open");
            seen.push_str(&String::from_utf8_lossy(&chunk));
        }
        assert!(
            seen.contains("the provider fell over"),
            "and it carries the provider's own words: {seen}"
        );
        assert!(seen.contains("\"scope\":\"turn\""), "{seen}");
    }

    /// An abort leaves the registry entry in place — this server still
    /// holds the lease — but it must stop taking messages: a steer
    /// accepted here would be read by a loop that has already returned.
    #[tokio::test]
    async fn a_message_after_an_abort_is_never_lost_to_a_finished_loop() {
        let harness = Harness::start(vec![
            sleep_call("call-1", "30"),
            answer("started after the abort"),
        ])
        .await;
        let (_, body) = harness
            .post("/api/sessions", json!({"prompt": "start a long tool"}))
            .await;
        let id = body["id"].as_str().unwrap().to_string();
        // Abort once the tool is genuinely running: the scripted
        // provider hands out its turns in order, so aborting before the
        // first one is fetched would leave the sleep for the *second*
        // turn to run.
        harness.until_working(&id).await;
        let (status, body) = harness
            .post(&format!("/api/sessions/{id}/abort"), json!({}))
            .await;
        assert_eq!(status, 200, "{body}");

        // The message either starts a turn or is refused in this
        // server's own words. What it must never be is `steering`.
        let mut started = false;
        for _ in 0..200 {
            let (status, body) = harness
                .post(
                    &format!("/api/sessions/{id}/message"),
                    json!({"text": "never mind, do this instead"}),
                )
                .await;
            if status == 200 {
                assert_eq!(body["fate"], "started", "a steer into a returned loop");
                started = true;
                break;
            }
            assert_eq!(status, 503, "{body}");
            assert_ne!(body["error"], json!(WATCHING_ONLY), "it is this server");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(started, "the message after the abort never ran");
        harness
            .transcript_once(&id, |page| {
                assistant_text(page).contains("started after the abort")
            })
            .await;
    }

    /// Two sends racing one idle session. Whatever the loser is told, it
    /// is not that the session belongs to another process — this server
    /// is the one holding it.
    #[tokio::test]
    async fn two_messages_racing_one_session_are_not_blamed_on_another_process() {
        let harness = Harness::start(vec![answer("first"), answer("second")]).await;
        let (_, body) = harness
            .post("/api/sessions", json!({"prompt": "one"}))
            .await;
        let id = body["id"].as_str().unwrap().to_string();
        harness
            .transcript_once(&id, |page| assistant_text(page).contains("first"))
            .await;
        harness.until_idle(&id).await;

        let path = format!("/api/sessions/{id}/message");
        let (left, right) = tokio::join!(
            harness.post(&path, json!({"text": "two"})),
            harness.post(&path, json!({"text": "three"})),
        );
        for (status, body) in [&left, &right] {
            assert_ne!(*status, 409, "{body}");
            assert_ne!(body["error"], json!(WATCHING_ONLY), "{body}");
        }
        assert!(
            [&left, &right]
                .iter()
                .any(|(status, body)| *status == 200 && body["fate"] == "started"),
            "one of them started the turn: {left:?} {right:?}"
        );
    }

    /// The same race without the network, where it is deterministic: the
    /// claim is in the registry before the slow part, so the second
    /// message is told what is actually happening.
    #[test]
    fn a_second_message_sees_this_server_starting_and_says_so() {
        let (drive, _dir) = drive_only();
        let Claim::Start(slot) = drive.claim("session", "one").unwrap() else {
            panic!("the first message claims the session");
        };
        assert!(drive.drives("session"), "claimed before the runtime exists");

        let refused = drive.claim("session", "two").unwrap_err();
        assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            refused.message().contains("already starting here"),
            "{}",
            refused.message()
        );
        assert_ne!(refused.message(), WATCHING_ONLY);

        // And the claim is given back by the guard, on every path out.
        drop(slot);
        assert!(!drive.drives("session"));
        assert!(matches!(
            drive.claim("session", "three").unwrap(),
            Claim::Start(_)
        ));
    }

    /// A task tool call that detaches. The child agent runs against the
    /// runtime's own resolver — the configured dead port — so it fails
    /// after the loop's transport retries and reports that failure as
    /// its completion notification; what this scripts is not the child's
    /// success but the *delivery machinery* the parent session needs
    /// alive after its own turn has ended.
    fn background_task_call(id: &str) -> Vec<ProviderEvent> {
        vec![
            ProviderEvent::ToolCallStarted {
                id: id.into(),
                name: "task".into(),
                item_id: None,
            },
            ProviderEvent::ToolCallCompleted {
                id: id.into(),
                name: "task".into(),
                input: json!({
                    "description": "background probe",
                    "prompt": "look around and report back",
                    "subagent_type": "explore",
                    "background": true,
                }),
            },
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::ToolUse,
                usage: Usage::default(),
            },
        ]
    }

    /// Like `transcript_once`, with the patience a background child
    /// needs: the child burns through the provider retry backoff before
    /// it completes, and this machine is allowed to be slow on top.
    async fn transcript_patiently(
        harness: &Harness,
        id: &str,
        ready: impl Fn(&Value) -> bool,
    ) -> Value {
        for _ in 0..600 {
            let page = harness.json(&format!("/api/sessions/{id}")).await;
            if ready(&page) {
                return page;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the transcript never arrived");
    }

    /// The heart of the issue: a driven turn detaches a task, the turn
    /// ends, and the completion must still find its way home with no web
    /// request involved — a `<task-notification>` user message on the
    /// parent's log and an assistant follow-up after it. Under the
    /// per-turn runtime the spawner was shut down at the turn boundary,
    /// so the child was cancelled and its notification channel died
    /// unread; this test hangs there.
    #[tokio::test]
    async fn a_background_completion_arrives_between_turns_as_a_follow_up_turn() {
        let harness = Harness::start(vec![
            background_task_call("task-1"),
            answer("kicked off"),
            answer("noted the task result"),
        ])
        .await;
        let (status, body) = harness
            .post("/api/sessions", json!({"prompt": "delegate the probe"}))
            .await;
        assert_eq!(status, 200, "{body}");
        let id = body["id"].as_str().unwrap().to_string();

        // Nothing else is posted: the follow-up turn is the server's own
        // doing, between turns.
        let page = transcript_patiently(&harness, &id, |page| {
            assistant_text(page).contains("noted the task result")
        })
        .await;

        let events = page["events"].as_array().unwrap();
        let delivered = events
            .iter()
            .position(|event| {
                event["type"] == "user_message" && event["notification"]["kind"] == "task"
            })
            .expect("the completion is a task-notification user message");
        let text = events[delivered].to_string();
        assert!(text.contains("<task-notification>"), "{text}");
        // And the follow-up is a turn of its own, after the first turn's
        // answer — not words smuggled into the running one.
        let first_answer = events
            .iter()
            .position(|event| event.to_string().contains("kicked off"))
            .expect("the first turn answered");
        let follow_up = events
            .iter()
            .position(|event| event.to_string().contains("noted the task result"))
            .expect("the follow-up answered");
        assert!(
            first_answer < delivered && delivered < follow_up,
            "turn, then delivery, then follow-up: {first_answer} {delivered} {follow_up}"
        );
    }

    /// The runtime is the session's, not the turn's: work detached in
    /// the first turn is still alive while a second driven turn runs,
    /// and its completion still lands afterwards. Under the per-turn
    /// runtime the first turn's shutdown killed the child on its way
    /// out, and no notification ever arrived.
    #[tokio::test]
    async fn work_spawned_in_one_turn_survives_the_next_and_still_reports() {
        let harness = Harness::start(vec![
            background_task_call("task-1"),
            answer("kicked off"),
            answer("second turn ran"),
            answer("task result noted"),
        ])
        .await;
        let (status, body) = harness
            .post("/api/sessions", json!({"prompt": "delegate the probe"}))
            .await;
        assert_eq!(status, 200, "{body}");
        let id = body["id"].as_str().unwrap().to_string();
        harness
            .transcript_once(&id, |page| assistant_text(page).contains("kicked off"))
            .await;
        harness.until_idle(&id).await;

        // A second turn, driven the ordinary way, while the child from
        // the first is still out there.
        let (status, body) = harness
            .post(
                &format!("/api/sessions/{id}/message"),
                json!({"text": "keep going"}),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["fate"], "started");

        // Both the second turn's answer and the delivered completion
        // arrive, in whichever order the child finishes.
        let page = transcript_patiently(&harness, &id, |page| {
            let text = assistant_text(page);
            text.contains("second turn ran") && text.contains("task result noted")
        })
        .await;
        let text = serde_json::to_string(&page["events"]).unwrap();
        assert!(text.contains("<task-notification>"), "{text}");
    }

    /// A session put into the store by "another process" — the seed is
    /// the real writer, so resuming it drives the same path a session
    /// from an earlier server life takes.
    fn seed_session(store: &SessionStore, cwd: &std::path::Path) -> String {
        let id = ilar::session::new_id();
        store
            .create(ilar::session::SessionMeta {
                session_id: id.clone(),
                parent_id: None,
                agent: "build".into(),
                // The configured custom model, so a resume resolves a
                // provider without a key in the environment.
                model: "custom/mock".into(),
                workspace: None,
                cwd: Some(cwd.to_path_buf()),
            })
            .unwrap();
        id
    }

    fn seed_child(store: &SessionStore, parent: &str) -> String {
        let id = ilar::session::new_id();
        store
            .create(ilar::session::SessionMeta {
                session_id: id.clone(),
                parent_id: Some(parent.to_string()),
                agent: "explore".into(),
                model: "custom/mock".into(),
                workspace: None,
                cwd: None,
            })
            .unwrap();
        id
    }

    fn notification(parent: &str, text: &str) -> Notification {
        Notification {
            parent_session_id: parent.to_string(),
            description: "recorded probe".into(),
            text: text.to_string(),
            is_error: false,
        }
    }

    /// Residue the outbox kept: a completion recorded for this tree
    /// before this engine existed is adopted when the engine starts and
    /// becomes a follow-up turn, with no web request asking for it.
    #[tokio::test]
    async fn adoption_requeues_outbox_completions_as_follow_up_turns() {
        let harness = Harness::start(vec![
            answer("resumed"),
            answer("noted the recovered result"),
        ])
        .await;
        let project = tempfile::tempdir().unwrap();
        let id = seed_session(&harness.store, project.path());
        ilar::outbox::record(
            &harness._dir.path().join("state").join("outbox"),
            &notification(
                &id,
                "<task-notification>\nrecovered probe result\n</task-notification>",
            ),
        );

        // The message is what wakes the engine; the recovered
        // completion rides in on its adoption.
        let (status, body) = harness
            .post(
                &format!("/api/sessions/{id}/message"),
                json!({"text": "hello again"}),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["fate"], "started");

        let page = transcript_patiently(&harness, &id, |page| {
            assistant_text(page).contains("noted the recovered result")
        })
        .await;
        let events = serde_json::to_string(&page["events"]).unwrap();
        assert!(events.contains("recovered probe result"), "{events}");
        assert!(events.contains("<task-notification>"), "{events}");
    }

    /// The head-of-line fix: a recovered completion for a child whose
    /// writer another process holds can only wait — minutes of lock
    /// retries and requeue backoff — and under the old serial consumer
    /// that wait held the engine's whole queue, own-session completions
    /// included. Here the driven session's own background completion
    /// must still land as a follow-up turn, inside this test's
    /// patience, while the child's delivery waits in its own lane.
    #[tokio::test]
    async fn a_stuck_foreign_delivery_does_not_stall_the_sessions_own_mail() {
        let harness = Harness::start(vec![
            background_task_call("task-1"),
            answer("kicked off"),
            answer("noted the task result"),
        ])
        .await;
        let project = tempfile::tempdir().unwrap();
        let id = seed_session(&harness.store, project.path());
        let child = seed_child(&harness.store, &id);
        ilar::outbox::record(
            &harness._dir.path().join("state").join("outbox"),
            &notification(
                &child,
                "<task-notification>\nstuck child result\n</task-notification>",
            ),
        );
        let held = harness
            .store
            .acquire_writer(&child)
            .expect("the child's writer lease");

        let (status, body) = harness
            .post(
                &format!("/api/sessions/{id}/message"),
                json!({"text": "delegate the probe"}),
            )
            .await;
        assert_eq!(status, 200, "{body}");

        let page = transcript_patiently(&harness, &id, |page| {
            assistant_text(page).contains("noted the task result")
        })
        .await;
        let text = serde_json::to_string(&page["events"]).unwrap();
        assert!(text.contains("<task-notification>"), "{text}");
        drop(held);
    }

    /// A scripted service tool call, as the events one provider turn
    /// carries for it (without the `TurnComplete` — a turn may chain
    /// several).
    fn service_call(id: &str, input: Value) -> Vec<ProviderEvent> {
        vec![
            ProviderEvent::ToolCallStarted {
                id: id.into(),
                name: "service".into(),
                item_id: None,
            },
            ProviderEvent::ToolCallCompleted {
                id: id.into(),
                name: "service".into(),
                input,
            },
        ]
    }

    /// The engine's lifetime, pinned at the service manager: a service
    /// one driven turn starts is still running when the next driven
    /// turn asks — the OS process probed alive by `status`, its output
    /// readable by `logs` — and stopping it is that later turn's
    /// decision, not the first turn's teardown.
    #[tokio::test]
    async fn a_service_started_in_one_turn_still_answers_in_the_next() {
        let start_turn = [
            service_call(
                "svc-1",
                json!({"action": "start", "name": "probe", "command": "echo ready; sleep 120"}),
            ),
            vec![ProviderEvent::TurnComplete {
                stop_reason: StopReason::ToolUse,
                usage: Usage::default(),
            }],
        ]
        .concat();
        let ask_turn = [
            service_call("svc-2", json!({"action": "status", "name": "probe"})),
            service_call("svc-3", json!({"action": "logs", "name": "probe"})),
            service_call("svc-4", json!({"action": "stop", "name": "probe"})),
            vec![ProviderEvent::TurnComplete {
                stop_reason: StopReason::ToolUse,
                usage: Usage::default(),
            }],
        ]
        .concat();
        let harness = Harness::start(vec![
            start_turn,
            answer("service is up"),
            ask_turn,
            answer("probe answered"),
        ])
        .await;

        let (status, body) = harness
            .post("/api/sessions", json!({"prompt": "start the probe"}))
            .await;
        assert_eq!(status, 200, "{body}");
        let id = body["id"].as_str().unwrap().to_string();
        harness
            .transcript_once(&id, |page| assistant_text(page).contains("service is up"))
            .await;
        harness.until_idle(&id).await;

        let (status, body) = harness
            .post(
                &format!("/api/sessions/{id}/message"),
                json!({"text": "is the probe still alive?"}),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["fate"], "started");

        let page = transcript_patiently(&harness, &id, |page| {
            assistant_text(page).contains("probe answered")
        })
        .await;
        let results: Vec<String> = page["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["type"] == "tool_result")
            .map(ToString::to_string)
            .collect();
        // The second turn's `status` probed the first turn's process
        // and found it alive — the survival this test exists to pin.
        assert!(
            results.iter().any(|r| r.contains("probe: running (pid")),
            "no status said running: {results:?}"
        );
        // Its `logs` answered with the output the service produced.
        assert!(
            results.iter().any(|r| r.contains("ready")),
            "no logs carried output: {results:?}"
        );
        // And the stop was the later turn's own doing.
        assert!(
            results.iter().any(|r| r.contains("stopped service")),
            "the probe was never stopped: {results:?}"
        );
    }

    /// The abort rule, at the registry: steerable before, refused after,
    /// and startable once the turn's slot is gone.
    #[test]
    fn an_abort_closes_the_entrys_ears_without_dropping_the_session() {
        let (drive, _dir) = drive_only();
        let Claim::Start(slot) = drive.claim("session", "one").unwrap() else {
            panic!("claimed");
        };
        let (steer, mut steers) = steer_channel();
        lock(&drive.running).get_mut("session").unwrap().steer = Some(steer);

        assert!(matches!(
            drive.claim("session", "steer me").unwrap(),
            Claim::Steered
        ));
        assert_eq!(steers.try_recv().unwrap().text, "steer me");

        assert_eq!(drive.abort("session").unwrap(), Fate::Aborted);
        assert!(
            drive.drives("session"),
            "the lease is still held until the turn returns"
        );
        let refused = drive.claim("session", "and this?").unwrap_err();
        assert!(
            refused.message().contains("stopping"),
            "{}",
            refused.message()
        );
        assert!(
            steers.try_recv().is_err(),
            "nothing was accepted into the closed loop"
        );

        drop(slot);
        assert!(matches!(
            drive.claim("session", "again").unwrap(),
            Claim::Start(_)
        ));
    }
}
