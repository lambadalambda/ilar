//! Turns on the library runtime, one live runtime per chat.
//!
//! A chat's runtime stays open between turns: its background subagents
//! keep running, and their completions come back to the chat as
//! follow-up turns the way they reach a TUI. Turns on one chat are
//! serialized; different chats run at once.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use ilar::agent::{LOOP_EVENT_CAPACITY, LoopEvent, TurnOutcome, loop_event_channel};
use ilar::config::Config;
use ilar::delivery::{Disposition, Parcel, disposition};
use ilar::provider::ProviderResolver;
use ilar::runtime::{RuntimeOptions, RuntimePlan, SessionRuntime};
use ilar::session::{ImageContent, SessionStore};
use ilar::subagent::{Notification, SubagentSpawner};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::GatewayConfig;
use crate::routes::RouteStore;

/// What a turn produced: the streamed text, and how the loop ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnReport {
    pub session_id: String,
    pub text: String,
    pub outcome: TurnOutcome,
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

/// A child's completion owed to a root session as a prompt.
#[derive(Debug, Clone)]
pub struct FollowUp {
    pub session_key: String,
    pub notification: Notification,
}

/// One chat's live runtime.
pub struct Seat {
    pub key: String,
    pub channel: String,
    pub chat_id: String,
    pub runtime: SessionRuntime,
    turn: tokio::sync::Mutex<()>,
}

pub struct Driver {
    config: Config,
    gateway: GatewayConfig,
    resolver: Arc<dyn ProviderResolver>,
    routes: Arc<RouteStore>,
    seats: Mutex<HashMap<String, Arc<Seat>>>,
    follow_ups: mpsc::Sender<FollowUp>,
    cancel: CancellationToken,
}

impl Driver {
    pub fn new(
        config: Config,
        gateway: GatewayConfig,
        resolver: Arc<dyn ProviderResolver>,
        routes: Arc<RouteStore>,
        follow_ups: mpsc::Sender<FollowUp>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            config,
            gateway,
            resolver,
            routes,
            seats: Mutex::new(HashMap::new()),
            follow_ups,
            cancel,
        }
    }

    pub fn outbox_dir(&self) -> PathBuf {
        self.config.state_dir().join("outbox")
    }

    /// The chat's runtime, opened on first use: its session resumed
    /// when the routes name one that still exists, created otherwise.
    pub fn seat(&self, key: &str, channel: &str, chat_id: &str) -> Result<Arc<Seat>> {
        if let Some(seat) = self.seats.lock().unwrap().get(key) {
            return Ok(seat.clone());
        }
        let known = self.routes.snapshot().session_for(key).map(str::to_string);
        let runtime = match self.open(known.clone()) {
            Ok(runtime) => runtime,
            // A route to a session that is gone (deleted, another state
            // dir) is a route to nothing: start over rather than refuse
            // the chat forever.
            Err(error) if known.is_some() => {
                log(&format!(
                    "{key}: session {} unusable ({error:#}); starting a new one",
                    known.unwrap_or_default()
                ));
                self.open(None)?
            }
            Err(error) => return Err(error),
        };
        self.routes
            .update(|routes| routes.bind(key, &runtime.session_id))?;
        let seat = Arc::new(Seat {
            key: key.to_string(),
            channel: channel.to_string(),
            chat_id: chat_id.to_string(),
            runtime,
            turn: tokio::sync::Mutex::new(()),
        });
        tokio::spawn(watch_notifications(
            seat.runtime.spawner.clone(),
            seat.runtime.store.clone(),
            self.outbox_dir(),
            seat.runtime.session_id.clone(),
            key.to_string(),
            self.follow_ups.clone(),
            self.cancel.child_token(),
        ));
        self.seats
            .lock()
            .unwrap()
            .insert(key.to_string(), seat.clone());
        Ok(seat)
    }

    fn open(&self, resume: Option<String>) -> Result<SessionRuntime> {
        let workspace = self.gateway.workspace(&self.config);
        std::fs::create_dir_all(&workspace)
            .with_context(|| format!("creating workspace {}", workspace.display()))?;
        RuntimePlan::resolve(
            &self.config,
            &RuntimeOptions {
                model: None,
                agent: self.gateway.agent.clone(),
                resume,
                cwd: workspace,
                // Nobody sits at a channel to answer a form: the tool is
                // left off and the model is told so on the spot.
                questions: false,
                project_instructions: None,
            },
        )?
        .start_with(&self.config, self.resolver.clone())
    }

    /// One turn on a seat; a second caller waits for the first.
    pub async fn run(
        &self,
        seat: &Seat,
        prompt: &str,
        images: &[ImageContent],
    ) -> std::result::Result<TurnReport, TurnError> {
        let _turn = seat.turn.lock().await;
        turn(&seat.runtime, prompt, images, self.cancel.child_token()).await
    }

    pub fn seats(&self) -> Vec<Arc<Seat>> {
        self.seats.lock().unwrap().values().cloned().collect()
    }

    /// Stop every seat's background work and services.
    pub async fn shutdown(&self) {
        for seat in self.seats() {
            seat.runtime.spawner.shutdown().await;
            seat.runtime.services.stop_all();
        }
    }
}

/// Run one turn and keep its text. The events are the loop's own; this
/// driver reads the answer and lets the rest go by.
pub async fn turn(
    runtime: &SessionRuntime,
    prompt: &str,
    images: &[ImageContent],
    cancel: CancellationToken,
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
        None,
    );
    tokio::pin!(turn);
    let mut text = String::new();
    let outcome = loop {
        tokio::select! {
            event = rx.recv() => match event {
                Some(LoopEvent::TextDelta(delta)) => text.push_str(&delta),
                Some(_) => {}
                None => break (&mut turn).await,
            },
            outcome = &mut turn => break outcome,
        }
    };
    while let Ok(event) = rx.try_recv() {
        if let LoopEvent::TextDelta(delta) = event {
            text.push_str(&delta);
        }
    }
    match outcome {
        Ok(outcome) => Ok(TurnReport {
            session_id: runtime.session_id.clone(),
            text,
            outcome,
        }),
        Err(error) if is_busy(&error) => Err(TurnError::Busy(format!("{error:#}"))),
        Err(error) => Err(TurnError::Failed(error)),
    }
}

/// The writer lease is an OS lock, and a refused lock is `WouldBlock`
/// somewhere in the chain — behind the loop's "never started" marker,
/// whose own chain skips the layer it wraps.
fn is_busy(error: &anyhow::Error) -> bool {
    let would_block = |cause: &(dyn std::error::Error + 'static)| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::WouldBlock)
    };
    error.chain().any(|cause| {
        would_block(cause)
            || cause
                .downcast_ref::<ilar::agent::TurnNeverStarted>()
                .is_some_and(|marker| marker.causes().any(would_block))
    })
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
    loop {
        while let Some(parcel) = queue.pop_front() {
            let notification = parcel.notification().clone();
            if notification.parent_session_id == session_id {
                let follow_up = FollowUp {
                    session_key: key.clone(),
                    notification,
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
                Disposition::Hold(parcel) => held.push(parcel),
                Disposition::Exhausted(stranded) => {
                    salvage(
                        &follow_ups,
                        &key,
                        &session_id,
                        &outbox_dir,
                        stranded,
                        "no session left to climb to",
                    )
                    .await;
                }
                Disposition::Salvage {
                    notification,
                    error,
                } => {
                    salvage(
                        &follow_ups,
                        &key,
                        &session_id,
                        &outbox_dir,
                        notification,
                        &error,
                    )
                    .await
                }
            }
        }
        let retry = async {
            if held.is_empty() {
                std::future::pending::<()>().await;
            } else {
                tokio::time::sleep(HOLD_RETRY).await;
            }
        };
        tokio::select! {
            () = cancel.cancelled() => return,
            next = live.recv() => match next {
                Some(notification) => queue.push_back(Parcel::fresh(notification)),
                None => return,
            },
            () = retry => queue.extend(held.drain(..)),
        }
    }
}

/// How long a held delivery waits before the next attempt.
const HOLD_RETRY: std::time::Duration = std::time::Duration::from_secs(5);

/// The delivery of last resort: the child's report goes to the chat's
/// own session as an error prompt, and the outbox entry is retired so
/// the next start does not announce it again.
async fn salvage(
    follow_ups: &mpsc::Sender<FollowUp>,
    key: &str,
    session_id: &str,
    outbox_dir: &std::path::Path,
    stranded: Notification,
    reason: &str,
) {
    ilar::outbox::retire(outbox_dir, &stranded);
    let _ = follow_ups
        .send(FollowUp {
            session_key: key.to_string(),
            notification: Notification {
                parent_session_id: session_id.to_string(),
                description: stranded.description.clone(),
                text: format!(
                    "<task-notification>\nTask \"{}\" finished but its result could not be delivered to the session that asked for it ({reason}). Its report:\n\n{}\n</task-notification>",
                    stranded.description, stranded.text
                ),
                is_error: true,
            },
        })
        .await;
}

pub fn log(message: &str) {
    eprintln!("{} {message}", chrono::Local::now().format("%H:%M:%S"));
}
