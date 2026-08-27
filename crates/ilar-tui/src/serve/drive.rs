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
use std::sync::{Arc, Mutex};

use axum::http::StatusCode;

use ilar::agent::{LOOP_EVENT_CAPACITY, SteerSender, loop_event_channel, steer_channel};
use ilar::config::Config;
use ilar::provider::ProviderResolver;
use ilar::runtime::{RuntimeOptions, RuntimePlan, SessionRuntime};
use ilar::session::{SessionStore, SessionTail, SessionWriter};
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
            Self::NotFound(_) | Self::NotDriving => StatusCode::NOT_FOUND,
            Self::Invalid(_) => StatusCode::BAD_REQUEST,
            Self::Failed(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub(crate) fn message(&self) -> String {
        match self {
            Self::Locked => WATCHING_ONLY.to_string(),
            Self::NotDriving => "no turn is running here that this server can abort".to_string(),
            Self::NotFound(message) | Self::Invalid(message) | Self::Failed(message) => {
                message.clone()
            }
        }
    }
}

/// A turn this process is running: where a steer goes, and what stops it.
struct RunningTurn {
    steer: SteerSender,
    cancel: CancellationToken,
    /// Which turn this is. A finishing turn must not evict the entry of
    /// the one that replaced it: `run_turn` releases the writer lease
    /// when it returns, so a message arriving in the moment before its
    /// task finishes cleaning up legitimately starts the next turn on
    /// the same session, and the loser of that race is whoever removes
    /// a key it no longer owns.
    epoch: u64,
}

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
    running: Arc<Mutex<HashMap<String, RunningTurn>>>,
    epochs: Arc<std::sync::atomic::AtomicU64>,
}

impl Drive {
    pub(crate) fn new(config: Config, store: SessionStore) -> Self {
        Self {
            config: Arc::new(config),
            store,
            resolver: None,
            running: Arc::new(Mutex::new(HashMap::new())),
            epochs: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
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
    /// difference between an abort control and a dot.
    pub(crate) fn drives(&self, id: &str) -> bool {
        self.running
            .lock()
            .expect("drive registry")
            .contains_key(id)
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
        self.spawn_turn(runtime, prompt, None);
        Ok(id)
    }

    /// Send a message to a session: steer the turn this process is
    /// running, or start one.
    pub(crate) async fn message(&self, id: &str, text: &str) -> Result<Fate, DriveError> {
        let text = text.trim().to_string();
        if text.is_empty() {
            return Err(DriveError::Invalid("an empty message says nothing".into()));
        }
        match self.submit_target(id) {
            SubmitTarget::Steer => {
                // The channel is unbounded and the turn drains it at
                // every step boundary; a send that fails means the turn
                // ended between the decision and here, so fall through
                // and start one rather than swallow the message.
                if self.steer(id, &text) {
                    return Ok(Fate::Steering);
                }
            }
            // Unreachable here by construction — every turn this process
            // starts is given a steer channel — but a turn that is
            // running and cannot take a message must not be started over
            // the top of itself.
            SubmitTarget::Queue => {
                return Err(DriveError::Failed(
                    "a turn is running here but cannot be steered".into(),
                ));
            }
            SubmitTarget::StartTurn => {}
        }

        self.ensure_exists(id).await?;
        let lease = self.acquire(id).await?;
        let options = RuntimeOptions {
            resume: Some(id.to_string()),
            cwd: std::env::current_dir()
                .map_err(|error| DriveError::Failed(format!("no working directory: {error}")))?,
            questions: false,
            ..RuntimeOptions::default()
        };
        let runtime = self.plan(options).await?;
        self.spawn_turn(runtime, text, Some(lease));
        Ok(Fate::Started)
    }

    /// Cancel the turn this process is running on that session.
    pub(crate) fn abort(&self, id: &str) -> Result<Fate, DriveError> {
        let running = self.running.lock().expect("drive registry");
        match running.get(id) {
            Some(turn) => {
                turn.cancel.cancel();
                Ok(Fate::Aborted)
            }
            None => Err(DriveError::NotDriving),
        }
    }

    /// The TUI's own steer-vs-start rule, asked with what serve can see.
    /// `busy` is false because the server has no post-turn settling
    /// state to protect: the registry entry *is* the turn's lifetime.
    fn submit_target(&self, id: &str) -> SubmitTarget {
        let driving = self.drives(id);
        decide::submit_target(
            &LoopState {
                turn_running: driving,
                steerable: driving,
                ..LoopState::default()
            },
            false,
        )
    }

    fn steer(&self, id: &str, text: &str) -> bool {
        let running = self.running.lock().expect("drive registry");
        running
            .get(id)
            .is_some_and(|turn| turn.steer.send(text.to_string()).is_ok())
    }

    /// A session that is not there is a 404, not a turn that fails five
    /// steps later. Cheap: the tail opens the file, it does not parse it.
    async fn ensure_exists(&self, id: &str) -> Result<(), DriveError> {
        let store = self.store.clone();
        let owned = id.to_string();
        let opened =
            tokio::task::spawn_blocking(move || SessionTail::open(&store, &owned).map(|_| ()))
                .await
                .map_err(|error| DriveError::Failed(format!("reader task failed: {error}")))?;
        opened.map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => {
                DriveError::NotFound(format!("session not found: {id}"))
            }
            std::io::ErrorKind::InvalidInput => DriveError::Invalid(error.to_string()),
            _ => DriveError::Failed(error.to_string()),
        })
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

    /// Register the turn and run it. The session is "driven" from the
    /// moment this returns, so the caller's response already tells the
    /// truth about what a follow-up message will do.
    fn spawn_turn(&self, runtime: SessionRuntime, prompt: String, lease: Option<SessionWriter>) {
        let (steer, steer_rx) = steer_channel();
        let cancel = CancellationToken::new();
        let id = runtime.session_id.clone();
        let epoch = self
            .epochs
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.running.lock().expect("drive registry").insert(
            id.clone(),
            RunningTurn {
                steer,
                cancel: cancel.clone(),
                epoch,
            },
        );
        let running = self.running.clone();
        let resolver = self
            .resolver
            .clone()
            .unwrap_or_else(|| runtime.resolver.clone());

        tokio::spawn(async move {
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
            // Only this turn's own entry: by now a later turn may have
            // taken the session, and removing its entry would leave a
            // running turn that cannot be steered or stopped.
            let mut running = running.lock().expect("drive registry");
            if running.get(&id).is_some_and(|turn| turn.epoch == epoch) {
                running.remove(&id);
            }
            drop(running);
            if let Err(error) = outcome {
                eprintln!("serve: turn on {id} failed: {error:#}");
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
            let state = super::super::http::ServeState {
                watcher,
                token: None,
                drive: Arc::new(drive),
            };
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
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
}
