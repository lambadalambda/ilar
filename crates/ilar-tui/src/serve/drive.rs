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
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard};

use axum::http::StatusCode;

use ilar::agent::{LOOP_EVENT_CAPACITY, SteerSender, loop_event_channel, steer_channel};
use ilar::config::Config;
use ilar::provider::ProviderResolver;
use ilar::runtime::{RuntimeOptions, RuntimePlan, SessionRuntime};
use ilar::session::{SessionStore, SessionWriter};
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
    epochs: Arc<std::sync::atomic::AtomicU64>,
    failures: broadcast::Sender<TurnFailure>,
}

impl Drive {
    pub(crate) fn new(config: Config, store: SessionStore) -> Self {
        Self {
            config: Arc::new(config),
            store,
            resolver: None,
            running: Arc::new(Mutex::new(HashMap::new())),
            epochs: Arc::new(std::sync::atomic::AtomicU64::new(0)),
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
        let runtime = self.plan(options).await?;
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
        let cwd = self.turn_cwd(id).await?;
        let lease = self.acquire(id).await?;
        let options = RuntimeOptions {
            resume: Some(id.to_string()),
            cwd,
            questions: false,
            ..RuntimeOptions::default()
        };
        let runtime = self.plan(options).await?;
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
        let epoch = self.epochs.fetch_add(1, Ordering::Relaxed);
        let cancel = CancellationToken::new();
        running.insert(
            id.to_string(),
            DrivenTurn {
                steer: None,
                cancel: cancel.clone(),
                stopping: false,
                epoch,
            },
        );
        TurnSlot {
            running: self.running.clone(),
            id: id.to_string(),
            epoch,
            cancel,
        }
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
        let store = self.store.clone();
        let owned = id.to_string();
        let acquired = tokio::task::spawn_blocking(move || store.acquire_writer(&owned))
            .await
            .map_err(|error| DriveError::Failed(format!("writer task failed: {error}")))?;
        acquired.map_err(|error| match error.kind() {
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
    /// "driven" since that claim; what this adds is the steer channel,
    /// so the caller's response tells the truth about what a follow-up
    /// message will do.
    fn spawn_turn(
        &self,
        runtime: SessionRuntime,
        prompt: String,
        lease: Option<SessionWriter>,
        slot: TurnSlot,
    ) {
        let (steer, steer_rx) = steer_channel();
        let cancel = slot.cancel.clone();
        let id = runtime.session_id.clone();
        {
            let mut running = lock(&self.running);
            // An abort that landed while the runtime resolved leaves the
            // entry `stopping`; the cancelled token below carries that,
            // and the ears stay closed.
            if let Some(turn) = running.get_mut(&id)
                && turn.epoch == slot.epoch
                && !turn.stopping
            {
                turn.steer = Some(steer);
            }
        }
        let failures = self.failures.clone();
        let resolver = self
            .resolver
            .clone()
            .unwrap_or_else(|| runtime.resolver.clone());

        tokio::spawn(async move {
            // The slot rides with the task: whether the turn returns,
            // fails or panics, its exit is what stops the session being
            // driven.
            let slot = slot;
            // The lease was this request's proof that nobody else owns
            // the session; `run_turn` takes its own, so it is released
            // here and nowhere earlier.
            drop(lease);
            let (events, mut receiver) = loop_event_channel(LOOP_EVENT_CAPACITY);
            // The loop publishes with backpressure, so somebody has to
            // read. Nothing here needs the events — the store and the
            // live scratch are the record, and the SSE reads those — but
            // an unread channel would stall the turn at 64 events.
            let drain = tokio::spawn(async move { while receiver.recv().await.is_some() {} });

            let outcome = ilar::agent::run_turn(
                resolver.as_ref(),
                &runtime.registry,
                &runtime.store,
                &id,
                &prompt,
                &[],
                Some(&runtime.system_prompt),
                runtime.loop_config.clone(),
                events,
                cancel,
                runtime.tool_ctx.clone(),
                Some(steer_rx),
            )
            .await;
            drain.abort();

            // Nothing outlives the turn: a background task with no
            // session to notify, or a service nobody will stop, is a
            // leak — the same shutdown `ilar exec` performs at exit.
            runtime.spawner.shutdown().await;
            runtime.services.stop_all();
            // The session stops being driven here, and not before the
            // lease `run_turn` returned is already released.
            drop(slot);
            if let Err(error) = outcome {
                let message = format!("the turn failed: {error:#}");
                eprintln!("serve: turn on {id}: {message}");
                // Whoever is watching this session hears it. Nobody
                // watching is not an error: the send fails when there is
                // no subscriber, which is the common case.
                let _ = failures.send(TurnFailure {
                    session_id: id,
                    message,
                });
            }
        });
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
