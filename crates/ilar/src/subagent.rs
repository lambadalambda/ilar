//! Task tool + subagent spawner — see meta/issues/task-tool-subagents.md.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::agent::{
    LOOP_EVENT_CAPACITY, LoopConfig, LoopEvent, LoopEventSender, TurnOutcome, loop_event_channel,
    run_turn,
};
use crate::config::{AgentDefinition, AgentWorkspaceMode, ProjectInstructions, system_prompt_for};
use crate::provider::ProviderResolver;
use crate::session::{ContentBlock, SessionMeta, SessionStore, TurnEnding, new_id};
use crate::tools::{
    Tool, ToolConcurrency, ToolContext, ToolFuture, ToolOutput, ToolRegistry, ToolStartObserver,
    WorkspaceAccess, WorkspacePermit,
};
use anyhow::Context;
use serde::{Deserialize, Serialize};

const NOTIFICATION_CAPACITY: usize = 64;
/// Slots on the child-activity broadcast. It carries *every* event of
/// every child at every depth — per-token deltas included — so it is
/// sized for a burst of several streaming children between two frames
/// of a reader that drains at 60Hz, not for a steady state. Lag is
/// survivable (the feed is display-only; delivery and the registry have
/// their own paths), but a gap shows, and it shows exactly when the
/// most is happening.
///
/// Four times the old size and no more: tokio allocates the ring
/// eagerly, a slot holds a whole `LoopEvent` (a coalesced delta reaches
/// 16 KiB, a tool result as much again), and every runtime builds one
/// whether or not anything ever subscribes.
pub const ACTIVITY_CAPACITY: usize = 1024;
/// How long a cancelled or stalled background child may take to finish
/// its graceful abort. That path only appends a partial step to the
/// session log and publishes the terminal event, so seconds are already
/// generous — the bound exists so a provider or tool that ignores
/// cancellation cannot wedge `shutdown`, which waits for these tasks.
const BACKGROUND_ABORT_GRACE: std::time::Duration = std::time::Duration::from_secs(5);
/// Poll interval and cap for a notification whose parent session is
/// locked by another turn. A lease that outlives ~3s is held by a turn
/// that is going to keep it, so the notification goes back to the queue
/// instead of spinning at 40 attempts a second until the heat death of
/// the universe.
const NOTIFICATION_LOCK_RETRY: std::time::Duration = std::time::Duration::from_millis(25);
const NOTIFICATION_LOCK_ATTEMPTS: usize = 120;
/// One round of waiting for a busy session to let go of its claim.
/// Between rounds the result is re-offered as a *steer*, which is the
/// point of the cap: the session that was mid-resume when this
/// delivery started may now be running a turn that can take the
/// result live, and taking it there beats queueing a second resume
/// behind the first — the two ✉ rows for one session.
const NOTIFICATION_CLAIM_WAIT: std::time::Duration = std::time::Duration::from_secs(3);
/// How long a routed delivery waits for the workspace before handing
/// the session back.
///
/// The session claim is held for the whole wait, so a mutable task
/// that keeps the lease pins every delivery to that session behind it,
/// not just this one. A requeue costs a round trip through the outbox;
/// an uncapped wait costs the session.
///
/// Generous for the same reason [`NOTIFICATION_CLAIM_ROUNDS`] is: in
/// the TUI a requeue is a held result, a warning notice and a prompt
/// for a keystroke, which an ordinary child turn must never provoke —
/// and a mutable task holding the lease across a build is exactly
/// that. This is not a fairness knob (the scheduler's queue is FIFO
/// and a requeue goes to the back of it); it is the ceiling that keeps
/// one stuck task from owning the session for the rest of the run.
const ROUTE_LEASE_WAIT: std::time::Duration = std::time::Duration::from_secs(10 * 60);
/// How many of those rounds before the result goes back to the user.
/// Generous on purpose: handing it back pauses delivery and asks for
/// a keystroke, which an ordinary child turn must never provoke. What
/// it rules out is the unbounded wait — a resume that runs for many
/// minutes used to hold the ✉ row and the "a task result is being
/// delivered; wait a moment" switch refusal for all of them.
const NOTIFICATION_CLAIM_ROUNDS: usize = 20;
/// The one wording for "make a worktree and name it here". Both refusals
/// that send the model there quote it, and so does the schema: a
/// corrective that drifts between sites is one the model has to learn
/// again at every site.
const WORKTREE_CORRECTION: &str = "`git worktree add ../ilar-task-<name> -b task/<name>`, then \
     pass \"workspace\": {\"cwd\": \"../ilar-task-<name>\", \"isolation\": \"git_worktree\"}";
/// Why a task that would have been detached ran in the turn instead.
/// Both cases are refusals for an explicit `background: true`, but a
/// default is ilar's choice rather than the caller's: demoting it keeps
/// the work happening, and the note keeps the result honest about which
/// path it took.
const BACKGROUND_DEMOTED_BY_CAPACITY: &str = "Ran in the foreground: tasks default to \
     background, but background capacity was full, so this one ran here instead of failing.";

/// A completed background task's notification — the synthetic user
/// message that re-invokes the parent loop. Serializable because the
/// durable outbox (`crate::outbox`) persists it as a JSONL line until
/// its delivery can be proven from the parent's session log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    pub parent_session_id: String,
    pub description: String,
    pub text: String,
    pub is_error: bool,
}

/// Take a lock whose guarded state cannot be torn by a panic. Every
/// mutex in this file guards a registry or map mutated with single
/// push/remove/insert operations, so a thread that panicked while
/// holding one left the data whole — but the poison flag would still
/// make every later `.unwrap()` panic, cascading one dead child into a
/// background runtime where no task can register, notify, or even run
/// its drop guards (a poisoned lock in a `Drop` during unwind aborts the
/// process). Ignoring the poison is the correct recovery here.
fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Debug, Clone)]
pub struct SubagentActivity {
    pub parent_session_id: String,
    pub parent_call_id: String,
    pub child_session_id: String,
    /// Which agent is doing the work. A surface learns from this that
    /// the call it is drawing spawned a subagent — true of `task`, and
    /// equally of a `task_message` that resumed one.
    pub agent: String,
    pub event: LoopEvent,
}

pub enum RouteOutcome {
    Complete,
    /// The ordinary climb: the target's log *took* the notification and
    /// ran a turn on it, and this is that turn's nested result rising to
    /// the target's own parent. The origin was delivered where it was
    /// addressed, so it owes the outbox nothing — the log holding it is
    /// what retires it.
    Propagate(Notification),
    /// The target could not take it at all, and no retry will change
    /// that: its workspace is gone, its context will not load. So this
    /// note does not follow the notification that was routed, it
    /// *replaces* it — that one reached no log, so its text (the
    /// finished child's only word) rides along inside this envelope,
    /// and the caller must retire it. The caller has it: it is what it
    /// handed in, which is also what `Salvage` hands back. Without that
    /// retire the entry stays undelivered for ever and every open
    /// re-adopts it, re-fails the same restore and manufactures this
    /// note anew: one root got the same phantom failure six times, once
    /// per open, measured 2026-09-15.
    Replace(Notification),
    Requeue(Notification),
}

/// Spawns child agent loops with their own sessions. Shared across a
/// session's turns (concurrency slot counter) and cloned into children
/// (depth+1) for nesting up to the depth cap.
pub struct SubagentSpawner {
    resolver: Arc<dyn ProviderResolver>,
    store: SessionStore,
    agents: Vec<AgentDefinition>,
    user_config_dir: std::path::PathBuf,
    /// Whether the workspace's own context file is trusted for this
    /// launch; inherited from the session that owns the spawner.
    project_instructions: ProjectInstructions,
    workspace_location: crate::tools::WorkspaceLocation,
    depth: usize,
    max_concurrent: usize,
    max_depth: usize,
    running: Arc<AtomicUsize>,
    active_sessions: Arc<Mutex<std::collections::HashSet<String>>>,
    active_sessions_changed: tokio::sync::watch::Sender<u64>,
    /// Tasks working right now, nested ones included: the registry is
    /// shared with every child spawner.
    running_tasks: Arc<Mutex<Vec<RunningTask>>>,
    /// What the parent has said to its children, keyed by child session.
    child_steers: ChildSteers,
    /// Background completions land here; the session owner consumes.
    notify_tx: tokio::sync::mpsc::Sender<Notification>,
    /// The single notification receiver, handed out by `subscribe`.
    notify_rx: Arc<Mutex<Option<tokio::sync::mpsc::Receiver<Notification>>>>,
    activity_tx: tokio::sync::broadcast::Sender<SubagentActivity>,
    stall_timeout: std::time::Duration,
    /// One round of a delivery's wait for a busy session; the total is
    /// this times [`NOTIFICATION_CLAIM_ROUNDS`]. A field only so a test
    /// can shrink it — the real value is seconds and the real total a
    /// minute, which no suite should sit through.
    claim_wait: std::time::Duration,
    /// Abort handles for detached background tasks.
    background_tasks: Arc<Mutex<BackgroundRegistry>>,
    workspace: crate::tools::WorkspaceScheduler,
    background_tool_timeout: std::time::Duration,
    /// When set, every notification publish is also appended to the
    /// durable outbox here before it enters the channel, so a process
    /// that dies with the notification in flight can requeue it at the
    /// next session open (`crate::outbox`).
    outbox_dir: Option<std::path::PathBuf>,
    loop_config: LoopConfig,
    /// Root session's service manager, shared with mutable child agents.
    services: Option<std::sync::Arc<crate::tools::service::ServiceManager>>,
    /// Models available for per-task overrides and the models tool.
    available_models: Vec<&'static crate::model::ModelInfo>,
    /// The session's secrets, for the listing tool in child registries
    /// and the notification turns this spawner runs.
    secrets: Option<crate::secrets::Secrets>,
    /// Whether child registries get the sudo tool (`agent.sudo`).
    sudo: bool,
    /// Paths withheld from every context this spawner builds — the
    /// children's, and the notification turns it runs on the parent
    /// session. See [`crate::tools::ToolContext::withheld`].
    withheld: Arc<[std::path::PathBuf]>,
}

/// The `agent` a background `bash` job registers under: not a subagent
/// at all, so a panel that counts agents or opens their transcripts has
/// to tell them apart. Named here, beside the one place that writes it.
pub const JOB_AGENT: &str = "job";

/// What a [`JOB_AGENT`] row runs, for a panel that shows some kinds
/// elsewhere: a service's watcher is on the services panel already.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Bash,
    ServiceWatch,
}

/// A subagent that is working right now, for anything that wants to
/// show live delegation — the TUI sidebar reads this every frame.
#[derive(Debug, Clone)]
pub struct RunningTask {
    pub session_id: String,
    /// Who spawned it — the session whose turn called the task tool,
    /// or, for a delivery, the resumed session's own parent. Lets a
    /// panel say whose child a nested agent is instead of flattening
    /// the tree.
    pub parent_session_id: String,
    pub description: String,
    pub agent: String,
    /// Set for a [`JOB_AGENT`] row, `None` for an agent's.
    pub job: Option<JobKind>,
    pub background: bool,
    /// A completion being delivered to this session — waiting for its
    /// current turn to end, or resuming it — rather than a task the
    /// model asked for. The mail on the panel.
    pub delivering: bool,
    pub started: std::time::Instant,
    /// Waiting for the workspace lease rather than working. A
    /// foreground task says this in its caller's tool row; a detached
    /// one has no row of its own, so it says it here — a mutable
    /// background task queued behind another one otherwise read as
    /// running on the panel.
    pub waiting: bool,
    /// Time since this task last made any progress, for the watchdog's
    /// marker on the panel. `None` for a task with no watchdog — a
    /// foreground child, a delivery. Filled by
    /// [`SubagentSpawner::running_tasks`] from the live clock, so a
    /// snapshot is never a stale number.
    pub quiet: Option<std::time::Duration>,
    /// Registry-assigned. Rows are removed by this, not the session
    /// id: a delivery waiting on a session and the turn it waits for
    /// are two rows with one session, and one ending must not erase
    /// the other.
    row: u64,
    /// The live progress clock `quiet` is read from. Private: the
    /// registry owns it and readers get the duration, so nobody
    /// outside can touch a running task's heartbeat.
    heartbeat: Option<crate::tools::Heartbeat>,
}

/// Removes its task from the running registry however the run ends —
/// completion, error, abort, or a dropped background future. It is also
/// the one handle to the live row: the registry entry is a snapshot
/// everyone else reads, and only the task that owns it may change it.
struct RunningTaskGuard {
    row: u64,
    registry: Arc<Mutex<Vec<RunningTask>>>,
}

impl RunningTaskGuard {
    fn update(&self, change: impl FnOnce(&mut RunningTask)) {
        // `row` is unique, so the first hit is the only hit.
        if let Some(task) = lock_unpoisoned(&self.registry)
            .iter_mut()
            .find(|task| task.row == self.row)
        {
            change(task);
        }
    }

    /// Say on the panel that this task is queued for the workspace
    /// rather than working, and take it back when the lease lands.
    fn set_waiting(&self, waiting: bool) {
        self.update(|task| task.waiting = waiting);
    }

    /// Hand the registry this task's progress clock, so the panel can
    /// say how long it has been quiet before the watchdog decides.
    fn watch(&self, heartbeat: &crate::tools::Heartbeat) {
        self.update(|task| task.heartbeat = Some(heartbeat.clone()));
    }
}

impl Drop for RunningTaskGuard {
    fn drop(&mut self) {
        lock_unpoisoned(&self.registry).retain(|task| task.row != self.row);
    }
}

/// What the parent has said to its children: the live steer channel of
/// every child turn that is running, and the messages sent but not yet
/// taken. Shared with every derived spawner, exactly like the running
/// registry, so a nested task is reachable from the spawner its own
/// parent holds.
/// Messages the parent sent to its children, by child session.
///
/// `dir` is what makes the tool's promise true across a quit: a parked
/// message is one the model was told "waits and is delivered at that
/// task's next resume", and process memory does not survive a restart
/// the way a task's session does. The mirror is rewritten whole under
/// the same lock as the change it records, so the file is never a
/// half-applied edit — and the list is a handful of short strings, so
/// whole is cheap.
#[derive(Clone, Default)]
struct ChildSteers {
    steers: Arc<Mutex<std::collections::HashMap<String, ChildSteer>>>,
    dir: Option<Arc<std::path::PathBuf>>,
}

#[derive(Default)]
struct ChildSteer {
    /// The running turn's steer channel; `None` once that turn ended.
    sender: Option<crate::agent::SteerSender>,
    /// What a run took into its prompt and has not committed yet. Still
    /// owed: an unstarted run hands these back, so they belong in the
    /// mirror until the run says it started.
    claimed: Vec<String>,
    /// Messages the child has not been seen to take. While its turn runs
    /// they are in flight; once it ends they wait for its next resume —
    /// the root rule, where an undelivered steer moves to the queue
    /// instead of vanishing with the channel.
    pending: Vec<String>,
}

impl ChildSteers {
    /// The same store, mirroring parked messages under `dir`.
    fn with_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.dir = Some(Arc::new(dir));
        self
    }

    fn mirror_path(dir: &std::path::Path, session_id: &str) -> std::path::PathBuf {
        dir.join(format!("{session_id}.json"))
    }

    /// The child's entry, its parked messages read off disk the first
    /// time this process touches it — which is how a message parked
    /// before a restart reaches the resume after it.
    fn entry<'a>(
        &self,
        steers: &'a mut std::collections::HashMap<String, ChildSteer>,
        session_id: &str,
    ) -> &'a mut ChildSteer {
        steers.entry(session_id.to_string()).or_insert_with(|| {
            let pending = self
                .dir
                .as_ref()
                .and_then(|dir| {
                    let path = Self::mirror_path(dir, session_id);
                    let read = std::fs::read(&path).ok()?;
                    match serde_json::from_slice::<Vec<String>>(&read) {
                        Ok(parked) => Some(parked),
                        // Unreadable: take it out of the way, or every
                        // later touch of this child pays to fail again.
                        Err(_) => {
                            let _ = std::fs::remove_file(&path);
                            None
                        }
                    }
                })
                .unwrap_or_default();
            ChildSteer {
                sender: None,
                claimed: Vec::new(),
                pending,
            }
        })
    }

    /// Write everything still owed to this child — what a run has
    /// claimed but not committed, then what is waiting behind it — or
    /// remove the file when nothing is owed. Called under the lock,
    /// after every change, so the file is never a half-applied edit.
    fn mirror(&self, session_id: &str, entry: &ChildSteer) {
        let Some(dir) = self.dir.as_ref() else {
            return;
        };
        let path = Self::mirror_path(dir, session_id);
        let owed: Vec<&String> = entry.claimed.iter().chain(entry.pending.iter()).collect();
        if owed.is_empty() {
            let _ = std::fs::remove_file(&path);
            return;
        }
        if let Ok(bytes) = serde_json::to_vec(&owed) {
            let _ = crate::memory::write_atomically(&path, &bytes);
        }
    }

    /// The receiver a child's turn runs with, and the run's claim on
    /// everything that was waiting for it.
    fn open(&self, session_id: &str) -> (crate::agent::SteerReceiver, ChildTurnSteer) {
        let (sender, receiver) = crate::agent::steer_channel();
        (receiver, self.begin(session_id, Some(sender)))
    }

    /// The same claim for a turn that cannot be steered — a routed
    /// notification, which is nonetheless a resume of that session and
    /// so carries what the session never read.
    fn adopt(&self, session_id: &str) -> ChildTurnSteer {
        self.begin(session_id, None)
    }

    /// Install the turn's channel and take its queue in one step: with
    /// both under the same lock, a message either goes to the turn that
    /// is starting or into the prompt that turn starts from, and never
    /// falls between the two.
    fn begin(&self, session_id: &str, sender: Option<crate::agent::SteerSender>) -> ChildTurnSteer {
        let mut steers = lock_unpoisoned(&self.steers);
        let entry = self.entry(&mut steers, session_id);
        entry.sender = sender;
        let queued = std::mem::take(&mut entry.pending);
        // Claimed, not delivered: the file keeps them until the run
        // says it committed them, so a crash mid-turn does not lose
        // what this call just took out of `pending`.
        entry.claimed = queued.clone();
        self.mirror(session_id, entry);
        drop(steers);
        ChildTurnSteer {
            session_id: session_id.to_string(),
            steers: self.clone(),
            queued,
        }
    }

    /// Hand a message to a running child's turn. False when no live
    /// channel took it: the caller then decides between resuming the
    /// task and holding the message for its next resume.
    ///
    /// A message in flight stays owed until the child's `Steered` event
    /// says it was read, so a crash between the two re-delivers it. The
    /// outbox makes the same trade for results, and for the same
    /// reason: saying a thing twice is recoverable, losing it is not.
    fn steer(&self, session_id: &str, text: String) -> bool {
        let mut steers = lock_unpoisoned(&self.steers);
        let Some(entry) = steers.get_mut(session_id) else {
            return false;
        };
        let Some(sender) = entry.sender.as_ref() else {
            return false;
        };
        // `task_message` is words only: a parent steering a child has
        // nothing attached to hand it.
        if sender.send(text.clone().into()).is_err() {
            return false;
        }
        entry.pending.push(text);
        self.mirror(session_id, entry);
        true
    }

    /// Hold a message for a child that cannot be steered right now.
    fn queue(&self, session_id: &str, text: String) {
        let mut steers = lock_unpoisoned(&self.steers);
        let entry = self.entry(&mut steers, session_id);
        // The same words twice are one message sent twice — a model
        // unsure the first was kept — and the child would read both.
        if !entry.pending.contains(&text) {
            entry.pending.push(text);
        }
        self.mirror(session_id, entry);
    }

    /// Whether this exact text is still waiting for the child. The
    /// message verb checks it after a resume it delegated declined, to
    /// say honestly that the message is parked rather than delivered.
    fn holds(&self, session_id: &str, text: &str) -> bool {
        let mut steers = lock_unpoisoned(&self.steers);
        let held = self
            .entry(&mut steers, session_id)
            .pending
            .iter()
            .any(|held| held == text);
        Self::prune(&mut steers, session_id);
        held
    }

    /// The run committed the prompt it built: what it claimed is read,
    /// so only what is still waiting stays owed.
    fn committed(&self, session_id: &str) {
        let mut steers = lock_unpoisoned(&self.steers);
        if let Some(entry) = steers.get_mut(session_id) {
            entry.claimed.clear();
            self.mirror(session_id, entry);
        }
        Self::prune(&mut steers, session_id);
    }

    /// The child took this message at a step boundary, so it is waiting
    /// for nothing — matched by text, the way the root's pending strip
    /// clears itself from the same `Steered` event.
    fn delivered(&self, session_id: &str, text: &str) {
        let mut steers = lock_unpoisoned(&self.steers);
        if let Some(entry) = steers.get_mut(session_id)
            && let Some(index) = entry.pending.iter().position(|held| held == text)
        {
            entry.pending.remove(index);
            self.mirror(session_id, entry);
        }
        Self::prune(&mut steers, session_id);
    }

    /// How many messages this task has not read yet.
    fn pending(&self, session_id: &str) -> usize {
        // Through `entry`, so a child this process has not touched yet
        // is counted from the mirror rather than reported as owing
        // nothing.
        let mut steers = lock_unpoisoned(&self.steers);
        let count = self.entry(&mut steers, session_id).pending.len();
        Self::prune(&mut steers, session_id);
        count
    }

    /// The turn is over: its channel is gone, and anything it took but
    /// never started goes back to the head of the queue, ahead of
    /// whatever was said while it was running.
    fn end(&self, session_id: &str, restored: Vec<String>) {
        let mut steers = lock_unpoisoned(&self.steers);
        // Through `entry`, not `get_mut`: a concurrent read may have
        // pruned this child away, and dropping `restored` here would
        // lose exactly the messages the mirror exists to keep.
        let entry = self.entry(&mut steers, session_id);
        entry.sender = None;
        entry.claimed.clear();
        entry.pending.splice(0..0, restored);
        self.mirror(session_id, entry);
        Self::prune(&mut steers, session_id);
    }

    /// A child with no channel and nothing waiting is not a child this
    /// map has anything to say about.
    fn prune(steers: &mut std::collections::HashMap<String, ChildSteer>, session_id: &str) {
        if steers.get(session_id).is_some_and(|entry| {
            entry.sender.is_none() && entry.pending.is_empty() && entry.claimed.is_empty()
        }) {
            steers.remove(session_id);
        }
    }
}

/// One child turn's hold on its task's messages: the queue it starts
/// from while it is starting, and the channel it reads while it runs.
/// However it ends, the channel goes; a run that never got as far as its
/// turn puts the queue back, because a lease it could not take must not
/// swallow what the parent said.
struct ChildTurnSteer {
    session_id: String,
    steers: ChildSteers,
    queued: Vec<String>,
}

impl ChildTurnSteer {
    /// The prompt this run actually starts from: what the task never
    /// read, then what the parent is asking now. An empty ask carries
    /// nothing of its own — the message verb's resume parks its text in
    /// the queue and starts the run with nothing else to say — so it
    /// adds no blank tail to the queue it delivers.
    fn prompt(&self, prompt: &str) -> String {
        if self.queued.is_empty() {
            return prompt.to_string();
        }
        self.queued
            .iter()
            .map(String::as_str)
            .chain((!prompt.is_empty()).then_some(prompt))
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// The turn is going ahead with that prompt: the messages in it are
    /// delivered, not waiting.
    fn started(&mut self) {
        self.queued.clear();
        self.steers.committed(&self.session_id);
    }
}

impl Drop for ChildTurnSteer {
    fn drop(&mut self) {
        self.steers
            .end(&self.session_id, std::mem::take(&mut self.queued));
    }
}

struct BackgroundTask {
    id: String,
    /// The child session this task drives, for the one caller that
    /// wants to stop *one* task: the panel and the focus view know a
    /// task by its session, and `id` is a registry key nothing outside
    /// ever sees. `None` for a background job, which has no session of
    /// its own — only cancel-all reaches those.
    session_id: Option<String>,
    handle: tokio::task::JoinHandle<()>,
    cancel: tokio_util::sync::CancellationToken,
}

#[derive(Default)]
struct BackgroundRegistry {
    tasks: Vec<BackgroundTask>,
    closed: bool,
}

impl SubagentSpawner {
    /// `project_instructions` is stated, never defaulted: a refused
    /// project file must stay refused for the agents a session delegates
    /// to, and a constructor default would let a new call site silently
    /// hand back exactly what the launch declined.
    ///
    /// Panics on a cwd that cannot be resolved — for tests and callers
    /// that own the path. A launch or resume whose cwd comes off disk
    /// wants [`SubagentSpawner::try_new`], which refuses instead of
    /// taking the process down with it.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        resolver: Arc<dyn ProviderResolver>,
        store: SessionStore,
        agents: Vec<AgentDefinition>,
        cwd: std::path::PathBuf,
        depth: usize,
        max_concurrent: usize,
        max_depth: usize,
        project_instructions: ProjectInstructions,
    ) -> Self {
        Self::try_new(
            resolver,
            store,
            agents,
            cwd,
            depth,
            max_concurrent,
            max_depth,
            project_instructions,
        )
        .unwrap_or_else(|error| panic!("{error:#}"))
    }

    /// The spawner, refusing a cwd that is not there.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        resolver: Arc<dyn ProviderResolver>,
        store: SessionStore,
        agents: Vec<AgentDefinition>,
        cwd: std::path::PathBuf,
        depth: usize,
        max_concurrent: usize,
        max_depth: usize,
        project_instructions: ProjectInstructions,
    ) -> anyhow::Result<Self> {
        let (notify_tx, notify_rx) = tokio::sync::mpsc::channel(NOTIFICATION_CAPACITY);
        let (activity_tx, _) = tokio::sync::broadcast::channel(ACTIVITY_CAPACITY);
        let workspace_location = crate::tools::WorkspaceLocation::try_shared(cwd)?;
        let workspace = crate::tools::WorkspaceScheduler::for_location(&workspace_location);
        let (active_sessions_changed, _) = tokio::sync::watch::channel(0);
        Ok(Self {
            notify_rx: Arc::new(Mutex::new(Some(notify_rx))),
            resolver,
            store,
            agents,
            user_config_dir: std::path::PathBuf::from("/nonexistent"),
            project_instructions,
            workspace_location,
            depth,
            max_concurrent,
            max_depth,
            running: Arc::new(AtomicUsize::new(0)),
            active_sessions: Arc::new(Mutex::new(std::collections::HashSet::new())),
            active_sessions_changed,
            running_tasks: Arc::new(Mutex::new(Vec::new())),
            child_steers: ChildSteers::default(),
            notify_tx,
            activity_tx,
            stall_timeout: std::time::Duration::from_secs(600),
            claim_wait: NOTIFICATION_CLAIM_WAIT,
            background_tasks: Arc::new(Mutex::new(BackgroundRegistry::default())),
            workspace,
            background_tool_timeout: std::time::Duration::from_secs(600),
            outbox_dir: None,
            loop_config: LoopConfig::default(),
            services: None,
            available_models: Vec::new(),
            secrets: None,
            sudo: false,
            withheld: Arc::from(Vec::new()),
        })
    }

    /// Override the background stall watchdog timeout (tests).
    pub fn with_stall_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.stall_timeout = timeout;
        self
    }

    /// Override one round of a delivery's wait for a busy session
    /// (tests). The real round is seconds and the whole budget a
    /// minute, which is the point of it — and far too long to sit
    /// through in a suite.
    pub fn with_claim_wait(mut self, wait: std::time::Duration) -> Self {
        self.claim_wait = wait;
        self
    }

    pub fn with_user_config_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.user_config_dir = dir;
        self
    }

    pub fn with_background_tool_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.background_tool_timeout = timeout;
        self
    }

    /// Persist every published notification to this directory until its
    /// delivery is provable from the parent session's log.
    pub fn with_outbox_dir(mut self, dir: std::path::PathBuf) -> Self {
        // A parked steer is this session owing a message elsewhere,
        // which is what the outbox directory is for; it keeps its own
        // subdirectory so neither reader has to skip the other's files.
        self.child_steers = std::mem::take(&mut self.child_steers).with_dir(dir.join("steers"));
        self.outbox_dir = Some(dir);
        self
    }

    /// Shrink the notification channel (tests): filling the real one
    /// would mean holding sixty-four children open at once, and what the
    /// capacity path does at its edge is worth testing cheaply.
    pub fn with_notification_capacity(mut self, capacity: usize) -> Self {
        let (notify_tx, notify_rx) = tokio::sync::mpsc::channel(capacity);
        self.notify_tx = notify_tx;
        self.notify_rx = Arc::new(Mutex::new(Some(notify_rx)));
        self
    }

    pub fn with_available_models(mut self, models: Vec<&'static crate::model::ModelInfo>) -> Self {
        self.available_models = models;
        self
    }

    pub fn with_secrets(mut self, secrets: crate::secrets::Secrets) -> Self {
        self.secrets = Some(secrets);
        self
    }

    pub fn with_sudo(mut self, sudo: bool) -> Self {
        self.sudo = sudo;
        self
    }

    /// Withhold these paths from every context this spawner builds.
    pub fn with_withheld(mut self, paths: Arc<[std::path::PathBuf]>) -> Self {
        self.withheld = paths;
        self
    }

    pub fn with_services(
        mut self,
        services: std::sync::Arc<crate::tools::service::ServiceManager>,
    ) -> Self {
        self.services = Some(services);
        self
    }

    pub fn with_loop_config(mut self, config: LoopConfig) -> Self {
        self.loop_config = config;
        self
    }

    pub fn background_tool_timeout(&self) -> std::time::Duration {
        self.background_tool_timeout
    }

    pub fn workspace(&self) -> crate::tools::WorkspaceScheduler {
        self.workspace.clone()
    }

    pub fn workspace_location(&self) -> crate::tools::WorkspaceLocation {
        self.workspace_location.clone()
    }

    /// Receiver for background-task notifications (single consumer;
    /// second call returns an already-closed receiver).
    pub fn subscribe(&self) -> tokio::sync::mpsc::Receiver<Notification> {
        lock_unpoisoned(&self.notify_rx)
            .take()
            .unwrap_or_else(|| tokio::sync::mpsc::channel::<Notification>(1).1)
    }

    pub fn subscribe_activity(&self) -> tokio::sync::broadcast::Receiver<SubagentActivity> {
        self.activity_tx.subscribe()
    }

    /// Abort every detached background task (the pending manager's
    /// cancel; quitting goes through `shutdown`).
    pub fn abort_all(&self) {
        let tasks = lock_unpoisoned(&self.background_tasks);
        for task in &tasks.tasks {
            task.cancel.cancel();
        }
    }

    /// Stop the one detached task driving `session_id`. `false` when no
    /// live task is driving it — it already finished, it is a
    /// foreground child whose caller owns its token, or it is a
    /// background job with no session — so the caller can say which
    /// instead of pretending it cancelled something.
    ///
    /// The task reports its own ending the way cancel-all's do: a
    /// `was cancelled` notification, held rather than delivered while
    /// notifications are paused.
    pub fn cancel_task(&self, session_id: &str) -> bool {
        let registry = self.live_background_tasks();
        let mut cancelled = false;
        for task in &registry.tasks {
            if task.session_id.as_deref() == Some(session_id) {
                task.cancel.cancel();
                cancelled = true;
            }
        }
        cancelled
    }

    /// The background registry with the finished tasks swept out. Every
    /// reader wants that — a finished handle is a task that is gone,
    /// and counting or cancelling one is a lie either way.
    fn live_background_tasks(&self) -> std::sync::MutexGuard<'_, BackgroundRegistry> {
        let mut registry = lock_unpoisoned(&self.background_tasks);
        registry.tasks.retain(|task| !task.handle.is_finished());
        registry
    }

    pub async fn shutdown(&self) {
        let tasks = {
            let mut registry = lock_unpoisoned(&self.background_tasks);
            registry.closed = true;
            for task in &registry.tasks {
                task.cancel.cancel();
            }
            registry
                .tasks
                .drain(..)
                .map(|task| task.handle)
                .collect::<Vec<_>>()
        };
        // Every task was cancelled at once, and each may spend up to
        // BACKGROUND_ABORT_GRACE finishing its abort: wait for them
        // together so quitting costs one grace, not one per child.
        let _ = futures::future::join_all(tasks).await;
    }

    /// The task results published to `session_id` that its log does not
    /// yet carry as a prompt: held by the driver, queued behind a turn,
    /// or in flight. Empty without an outbox — there is nothing durable
    /// to ask, and the listing then says nothing about delivery.
    pub fn undelivered_results(&self, session_id: &str) -> Vec<Notification> {
        match self.outbox_dir.as_deref() {
            Some(dir) => crate::outbox::undelivered(&self.store, dir, session_id),
            None => Vec::new(),
        }
    }

    /// Number of live detached background tasks.
    pub fn running_background(&self) -> usize {
        self.live_background_tasks().tasks.len()
    }

    pub fn resolver(&self) -> Arc<dyn ProviderResolver> {
        self.resolver.clone()
    }

    pub fn agents(&self) -> &[AgentDefinition] {
        &self.agents
    }

    /// A spawner sharing every collaborator with this one — slot counter,
    /// session claims, notification channel, background registry — but
    /// bound to another workspace and depth. The only place the fields are
    /// enumerated: both derivation sites go through it, so a new field
    /// cannot be forgotten on one of them.
    fn derived(
        &self,
        workspace_location: crate::tools::WorkspaceLocation,
        workspace: crate::tools::WorkspaceScheduler,
        depth: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            resolver: self.resolver.clone(),
            store: self.store.clone(),
            agents: self.agents.clone(),
            user_config_dir: self.user_config_dir.clone(),
            project_instructions: self.project_instructions,
            workspace_location,
            depth,
            max_concurrent: self.max_concurrent,
            max_depth: self.max_depth,
            running: self.running.clone(),
            active_sessions: self.active_sessions.clone(),
            active_sessions_changed: self.active_sessions_changed.clone(),
            running_tasks: self.running_tasks.clone(),
            child_steers: self.child_steers.clone(),
            notify_tx: self.notify_tx.clone(),
            notify_rx: self.notify_rx.clone(),
            activity_tx: self.activity_tx.clone(),
            stall_timeout: self.stall_timeout,
            claim_wait: self.claim_wait,
            background_tasks: self.background_tasks.clone(),
            workspace,
            background_tool_timeout: self.background_tool_timeout,
            outbox_dir: self.outbox_dir.clone(),
            loop_config: self.loop_config.clone(),
            services: self.services.clone(),
            available_models: self.available_models.clone(),
            secrets: self.secrets.clone(),
            sudo: self.sudo,
            withheld: self.withheld.clone(),
        })
    }

    /// The tool registry an agent runs with under this spawner. A
    /// read-only agent gets the enforced read-only set — no delegation,
    /// no shell; a mutable one gets the builtins plus delegation,
    /// services and the model listing. Either way the agent definition's
    /// `tools` allowlist narrows the result. Called on the spawner the
    /// agent will itself delegate through, so its task tool and its
    /// services can never come from two different spawners.
    fn agent_registry(
        self: &Arc<Self>,
        agent: &AgentDefinition,
    ) -> Result<ToolRegistry, crate::tools::DuplicateToolError> {
        let registry = match agent.workspace_mode {
            AgentWorkspaceMode::ReadOnly => ToolRegistry::read_only(),
            AgentWorkspaceMode::Mutable => {
                // At the depth limit a task call can only be refused, so
                // the tools that make one are left off: their schema is a
                // tenth of every request this agent sends.
                let registry = if self.depth < self.max_depth {
                    ToolRegistry::builtin().with_subagents(self.clone())?
                } else {
                    ToolRegistry::builtin()
                };
                let registry = match self.services.clone() {
                    Some(services) => registry.with_services(services)?,
                    None => registry,
                };
                let registry = registry.with_models(self.available_models.clone())?;
                let registry = match &self.secrets {
                    Some(secrets) => registry.with_secrets(secrets.store().clone())?,
                    None => registry,
                };
                if self.sudo {
                    registry.with_sudo()?
                } else {
                    registry
                }
            }
        };
        Ok(match &agent.tools {
            Some(tools) => registry.restricted_to(tools),
            None => registry,
        })
    }

    /// The system prompt an agent runs with under this spawner: the
    /// context of the workspace it will work in, plus its own prompt.
    fn agent_system_prompt(
        &self,
        agent: &AgentDefinition,
        cwd: &std::path::Path,
    ) -> anyhow::Result<String> {
        Ok(crate::runtime::with_agent_prompt(
            system_prompt_for(&self.user_config_dir, cwd, self.project_instructions)?.prompt,
            agent,
        ))
    }

    /// Run one subagent task; returns its final text as the tool output.
    pub async fn run_task(self: &Arc<Self>, input: TaskInput, ctx: &ToolContext) -> ToolOutput {
        self.run_task_observed(input, ctx, None, InStep::Yes).await
    }

    /// `in_step` says whether a foreground run would hold a step of the
    /// model's: every model call does; a resume the person started from
    /// the task's own view runs beside the conversation and holds none.
    async fn run_task_observed(
        self: &Arc<Self>,
        input: TaskInput,
        ctx: &ToolContext,
        mut on_start: Option<ToolStartObserver>,
        in_step: InStep,
    ) -> ToolOutput {
        if self.depth >= self.max_depth {
            return ToolOutput::error(format!(
                "task: nesting limit reached (depth {} of {}); do this work directly with your \
                 own tools instead of spawning another agent",
                self.depth, self.max_depth
            ));
        }
        let Some(agent) = self.agents.iter().find(|a| a.name == input.subagent_type) else {
            let available: Vec<&str> = self.agents.iter().map(|a| a.name.as_str()).collect();
            return ToolOutput::error(format!(
                "unknown subagent_type {:?}; available: {}",
                input.subagent_type,
                available.join(", ")
            ));
        };
        let workspace_access = match agent.workspace_mode {
            AgentWorkspaceMode::Mutable => WorkspaceAccess::Mutating,
            AgentWorkspaceMode::ReadOnly => WorkspaceAccess::ReadOnly,
        };
        // The one place `background` stops being a maybe. Omitted, it
        // means "detach and tell me when it lands", whatever the agent:
        // a foreground task blocks the parent's *conversation* — a
        // message the person sends meanwhile is a steer, read at the
        // parent's next step, which is after the task returns — and
        // nothing about a mutable task needs that. Its edits still land
        // in order: in the parent's own checkout it holds the write
        // lease, and the parent's edits are refused until it reports
        // rather than holding the parent's step. An explicit
        // false is the caller saying it is blocked on the answer, which
        // is Codex's `wait_agent` and as rare. Everything below sees a
        // bool, so capacity and notification wiring never have to ask
        // what the caller meant.
        let background_explicit = input.background.is_some();
        let mut background = input.background.unwrap_or(true);
        let mut background_demoted: Option<&'static str> = None;
        let child_location = match &input.workspace {
            Some(workspace) => {
                let TaskWorkspaceIsolation::GitWorktree = workspace.isolation;
                match crate::tools::WorkspaceLocation::validated_git_worktree(
                    &ctx.location,
                    workspace.cwd.clone(),
                )
                .await
                {
                    Ok(location) => location,
                    Err(error) => {
                        return ToolOutput::error(format!(
                            "invalid task workspace {:?}: {error:#}. workspace.cwd must already be \
                             a registered Git worktree — of the session's repository, or of a \
                             repository beneath the session cwd: {WORKTREE_CORRECTION}. A path in \
                             no Git repository can never be a workspace: omit workspace to run in \
                             the current checkout.",
                            workspace.cwd
                        ));
                    }
                }
            }
            None => ctx.location.clone(),
        };
        let same_workspace = child_location.id() == ctx.location.id();
        let cross_workspace_nested = !same_workspace && ctx.has_workspace_lease();
        if !same_workspace
            && ctx
                .workspace_ancestry
                .iter()
                .any(|id| id == child_location.id())
        {
            return ToolOutput::error(
                "task workspace is already held by an ancestor; finish the intervening task before returning to it",
            );
        }
        if workspace_access == WorkspaceAccess::Mutating
            && same_workspace
            && ctx.has_workspace_lease()
        {
            return ToolOutput::error(format!(
                "nested mutable tasks cannot reuse their parent checkout; this one is held for the \
                 whole of the task you are running in. Run it in a sibling worktree instead: \
                 {WORKTREE_CORRECTION}. If the task only needs to read, pass a read-only \
                 subagent_type and omit workspace."
            ));
        }
        let notification_permit = if background {
            match self.notify_tx.clone().try_reserve_owned() {
                Ok(permit) => Some(permit),
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) if background_explicit => {
                    return ToolOutput::error(
                        "too many background tasks and jobs are running or waiting to be delivered: run this in the foreground, or end your turn and start it once a notification has arrived",
                    );
                }
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    background = false;
                    background_demoted = Some(BACKGROUND_DEMOTED_BY_CAPACITY);
                    None
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    return ToolOutput::error("background notification receiver is closed");
                }
            }
        } else {
            None
        };
        // Read `background` only once the capacity demotion above has had
        // its say: a defaulted background task that fell back to the
        // foreground must inherit the parent's lease like any other
        // foreground child, not go take one of its own.
        //
        // A background child never shares the parent's lease: the Arc'd
        // permit would keep a write lock alive past the parent task's
        // end. Reads are advisory now, so a detached reader needs no
        // lease at all; it acquires its own free one.
        let inherited_lease = if same_workspace && !background {
            match ctx.workspace_coverage(workspace_access) {
                crate::tools::WorkspaceCoverage::Covered => ctx.workspace_lease.clone(),
                crate::tools::WorkspaceCoverage::Absent => None,
                crate::tools::WorkspaceCoverage::Incompatible => {
                    return ToolOutput::error(
                        "mutable task cannot run inside a read-only child workspace",
                    );
                }
            }
        } else {
            None
        };
        let child_workspace = ctx.workspace.scoped(&child_location);
        // Blocked on *this* task, not on another job as well: behind a
        // detached holder the wait would hold the model's step — and
        // every message from the person — for as long as that job runs.
        // The refusal the executor gives edit and bash, with the ways out
        // a task has, and before anything is set up for a run that will
        // not happen. Only an explicit false: a task the capacity demoted
        // did leave background out. A holder inside a step (a sibling)
        // ends with it, and is waited for as before.
        if input.background == Some(false)
            && in_step == InStep::Yes
            && inherited_lease.is_none()
            && workspace_access == WorkspaceAccess::Mutating
            && let Some(holder) = child_workspace.detached_holder()
        {
            let worktree = if input.task_id.is_none() {
                ", or give it a worktree of its own"
            } else {
                ""
            };
            return ToolOutput::error(format!(
                "task: not run — this checkout is held by {holder} until it ends. Leave \
                 background out to queue this behind it (the result arrives as a \
                 notification){worktree}."
            ));
        }
        let system_prompt = match self.agent_system_prompt(agent, child_location.cwd()) {
            Ok(prompt) => prompt,
            Err(error) => {
                return ToolOutput::error(format!("loading subagent context: {error:#}"));
            }
        };

        let mut active_session = match &input.task_id {
            Some(id) => match self.claim_session(id) {
                Some(claim) => Some(claim),
                None => {
                    return ToolOutput::error(format!(
                        "task {id} is running right now: a task_message to it is read at its next step or next resume"
                    ));
                }
            },
            None => None,
        };

        // Concurrency slot: Claude Code semantics — over cap is a soft
        // error the model must not retry.
        if self.running.fetch_add(1, Ordering::SeqCst) >= self.max_concurrent {
            self.running.fetch_sub(1, Ordering::SeqCst);
            return ToolOutput::error(format!(
                "task: already running {} tasks, which is the limit. Calling again now fails \
                 the same way. Wait for one to finish — `tasks` lists them, and a result \
                 reaches you on its own — and spawn then.",
                self.max_concurrent
            ));
        }
        let _guard = SlotGuard(self.running.clone());

        // Session: resume task_id if given and loadable, else a fresh child.
        let session_id = match &input.task_id {
            Some(id) => match self.store.load(id) {
                Ok(session) => {
                    let Some(meta) = session.meta() else {
                        return ToolOutput::error(format!(
                            "resuming task session {id:?}: session has no metadata"
                        ));
                    };
                    if meta.agent != input.subagent_type {
                        return ToolOutput::error(format!(
                            "resuming task session {id:?}: it ran as agent {:?}, not {:?}; resume \
                             it with that subagent_type or start a new task",
                            meta.agent, input.subagent_type
                        ));
                    }
                    if meta.parent_id.as_deref() != Some(ctx.session_id.as_str()) {
                        return ToolOutput::error(format!(
                            "resuming task session {id:?}: persisted parent does not match the invoking session"
                        ));
                    }
                    match &meta.workspace {
                        Some(persisted) => {
                            let restored = if persisted == &ctx.location {
                                ctx.location.clone()
                            } else if input.workspace.is_none() {
                                // Name the worktree, or say plainly there is
                                // none to name: pointing at task_message
                                // for a checkout it cannot find either
                                // would send the model round in a loop.
                                return ToolOutput::error(match persisted.isolation() {
                                    crate::tools::WorkspaceIsolation::GitWorktree { .. } => {
                                        format!(
                                            "resuming task {id:?}: it ran in its own worktree, {} — pass it as workspace, or use task_message, which finds it from the task's metadata",
                                            persisted.cwd().display()
                                        )
                                    }
                                    _ => format!(
                                        "resuming task {id:?}: it ran in another checkout, {}, and cannot be resumed from this one; start a new task here instead",
                                        persisted.cwd().display()
                                    ),
                                });
                            } else {
                                match crate::tools::WorkspaceLocation::revalidate(
                                    &ctx.location,
                                    persisted,
                                )
                                .await
                                {
                                    Ok(location) => location,
                                    Err(error) => {
                                        return ToolOutput::error(format!(
                                            "resuming task session {id:?}: persisted workspace is invalid: {error:#}"
                                        ));
                                    }
                                }
                            };
                            if restored != *persisted {
                                return ToolOutput::error(format!(
                                    "resuming task session {id:?}: persisted workspace metadata does not match its canonical location"
                                ));
                            }
                            if restored != child_location {
                                return ToolOutput::error(format!(
                                    "resuming task session {id:?}: workspace does not match; provide its validated worktree"
                                ));
                            }
                        }
                        None if input.workspace.is_some() => {
                            return ToolOutput::error(format!(
                                "resuming task session {id:?}: session has no workspace metadata and cannot adopt an isolated workspace"
                            ));
                        }
                        None => {}
                    }
                    id.clone()
                }
                Err(error) => {
                    return ToolOutput::error(format!(
                        "resuming task session {id:?}: {error}. Task ids come from task results \
                         and the tasks tool; never invent one"
                    ));
                }
            },
            None => {
                let id = new_id();
                let requested_variant = input.reasoning.clone().map(TaskVariant::from_input);
                let (model, child_variant) = match (&input.model, &agent.model) {
                    (Some(model), _) | (None, Some(model)) => (model.clone(), requested_variant),
                    (None, None) => match self.store.load(&ctx.session_id) {
                        Ok(parent) => (
                            parent.effective_model(),
                            requested_variant.or_else(|| {
                                parent.effective_variant().map(TaskVariant::from_parent)
                            }),
                        ),
                        Err(error) => {
                            return ToolOutput::error(format!(
                                "loading parent session {:?}: {error}",
                                ctx.session_id
                            ));
                        }
                    },
                };
                if input.model.is_some() {
                    let known = self
                        .available_models
                        .iter()
                        .any(|candidate| candidate.full_id() == model)
                        || (self.available_models.is_empty()
                            && crate::model::find(&model).is_some());
                    if !known {
                        return ToolOutput::error(format!(
                            "unknown or unavailable model {model:?}; call the models tool for the list"
                        ));
                    }
                    if let Err(error) = self.resolver.resolve_provider(&model) {
                        return ToolOutput::error(format!(
                            "no provider configured for {model}: {error:#}"
                        ));
                    }
                }
                if let Some(variant) = &child_variant
                    && crate::model::variant_options(&model, Some(&variant.id)).is_err()
                {
                    return ToolOutput::error(variant.rejection(&model));
                }
                let created = self.store.create(SessionMeta {
                    session_id: id.clone(),
                    parent_id: Some(ctx.session_id.clone()),
                    agent: input.subagent_type.clone(),
                    model,
                    workspace: Some(child_location.clone()),
                    // Children are never listed, so there is no listing
                    // for a launch directory to group.
                    cwd: None,
                });
                match created {
                    Ok(mut session) => {
                        let child_model = session.effective_model();
                        if let Some(variant) = child_variant
                            && let Err(error) =
                                session.append(crate::session::SessionEvent::ModelChange {
                                    id: new_id(),
                                    model: child_model,
                                    variant: Some(variant.id),
                                    ts: chrono::Utc::now(),
                                })
                        {
                            drop(session);
                            rollback_created_session(&self.store, &id);
                            return ToolOutput::error(format!(
                                "persisting subagent reasoning variant: {error}"
                            ));
                        }
                        drop(session);
                    }
                    Err(error) => {
                        return ToolOutput::error(format!("creating subagent session: {error}"));
                    }
                };
                id
            }
        };
        if active_session.is_none() {
            active_session = self.claim_session(&session_id);
        }
        let _active_session = active_session.expect("new session id must be unique");
        // From here the child is working: everything below either runs
        // it or moves the guard into the task that will.
        let running_task = self.register_running(RunningTask {
            session_id: session_id.clone(),
            parent_session_id: ctx.session_id.clone(),
            description: input.description.clone(),
            agent: agent.name.clone(),
            job: None,
            background,
            delivering: false,
            started: std::time::Instant::now(),
            // Registry-owned: assigned and maintained by
            // `register_running` and the guard it hands back.
            row: 0,
            waiting: false,
            quiet: None,
            heartbeat: None,
        });
        // The channel and the queue in one step, under the claim taken
        // above: from here a message reaches the turn that is starting,
        // and one that arrived before it heads that turn's prompt —
        // anything the parent said while the task's last turn was ending
        // never reached it, and the root rule is that such a steer waits
        // in the queue rather than vanishing. A task's queue is the
        // prompt of its next run, which this is.
        let (steer_rx, mut child_steer) = self.child_steers.open(&session_id);
        let prompt = child_steer.prompt(&input.prompt);
        let parent_call_id = ctx.call_id.clone().unwrap_or_default();

        // The child delegates one level deeper, sharing this spawner's
        // slot counter, session claims and notification channel.
        let child_spawner = self.derived(
            child_location.clone(),
            child_workspace.clone(),
            self.depth + 1,
        );
        let registry = match child_spawner.agent_registry(agent) {
            Ok(registry) => registry,
            Err(error) => {
                return ToolOutput::error(format!("building child tool registry: {error}"));
            }
        };
        let mut workspace_ancestry = ctx.workspace_ancestry.clone();
        if !workspace_ancestry
            .iter()
            .any(|id| id == child_location.id())
        {
            workspace_ancestry.push(child_location.id().clone());
        }
        let mut child_ctx = ToolContext {
            cwd: child_location.cwd().to_path_buf(),
            session_id: session_id.clone(),
            call_id: ctx.call_id.clone(),
            depth: self.depth + 1,
            subagent: Some(child_spawner),
            workspace: child_workspace.clone(),
            location: child_location.clone(),
            workspace_lease: None,
            workspace_ancestry,
            cancel: ctx.cancel.clone(),
            output_tail: None,
            // Not inherited from the parent: the child's turn sets this
            // from the child session's own model, which is the one that
            // will be looking at whatever its tools return.
            vision: false,
            // Empty, not the parent's: the child model has seen nothing
            // of the workspace, so its first edit reads the file itself.
            seen_files: crate::tools::SeenFiles::default(),
            // Inherited: a child's oversized output is worth keeping for
            // the same reason its parent's is.
            spill_dir: ctx.spill_dir.clone(),
            // Inherited: a child's bash asks the same person — named,
            // so a prompt that appears while several children run says
            // which one wants the secret.
            secrets: ctx
                .secrets
                .clone()
                .map(|secrets| secrets.for_agent(&agent.name)),
            // Inherited: a child of a seat in a room is in the same
            // room, and delegating the read is still the read.
            withheld: ctx.withheld.clone(),
            // Inherited for a foreground child, whose every event is
            // its blocked caller's progress; the background branch
            // below replaces it with the watchdog of its own.
            heartbeat: ctx.heartbeat.clone(),
            // The child's turn sets its own from the steers it takes.
            steers: None,
            // A foreground child works for its caller's owner; the
            // background branch below makes the task its own.
            detached_owner: ctx.detached_owner.clone(),
        };

        if background {
            // Detached: run the child on a spawned task with a stall
            // watchdog; completion lands as a notification for the parent
            // loop; the tool call returns immediately.
            let spawner = Arc::clone(self);
            let description = input.description.clone();
            let parent_session_id = ctx.session_id.clone();
            let stall_timeout = self.stall_timeout;
            // A child of the owner's token, so one token stands for "this
            // task should stop": its owner stopping cancels it, and
            // `abort_all`/`shutdown` can still cancel it alone.
            let root_cancel = detached_owner(ctx);
            let background_cancel = root_cancel.child_token();
            let task_cancel = background_cancel.clone();
            let workspace = child_workspace.clone();
            let parent_location = ctx.location.clone();
            let leased_location = child_location.clone();
            let mut background_registry = lock_unpoisoned(&self.background_tasks);
            if background_registry.closed {
                return ToolOutput::error("background runtime is shutting down");
            }
            background_registry
                .tasks
                .retain(|task| !task.handle.is_finished());
            let registry_id = new_id();
            let task_registry_id = registry_id.clone();
            let task_registry = self.background_tasks.clone();
            let activity = ActivityPublisher {
                tx: self.activity_tx.clone(),
                agent: agent.name.clone(),
                steers: self.child_steers.clone(),
                parent_session_id: parent_session_id.clone(),
                parent_call_id,
                child_session_id: session_id.clone(),
            };
            let returned_session_id = session_id.clone();
            let (registered_tx, registered_rx) = tokio::sync::oneshot::channel();
            // The reservation becomes a guard here, not at the reserve
            // above: the early returns between the two report their
            // errors synchronously, and a guard armed that early would
            // announce an abnormal death for a task that never existed.
            let reserved = ReservedNotification::new(
                notification_permit.expect("reserved for background task"),
                parent_session_id.clone(),
                description.clone(),
                self.outbox_dir.clone(),
            )
            .for_session(session_id.clone(), self.child_steers.clone());
            let handle = tokio::spawn(async move {
                if registered_rx.await.is_err() {
                    // Never admitted: the spawner reported the failure
                    // synchronously, so no notification may claim a task
                    // died here.
                    reserved.disarm();
                    return;
                }
                let reserved = reserved;
                let _background_task = BackgroundTaskGuard {
                    id: task_registry_id,
                    registry: task_registry,
                };
                let _active_session = _active_session;
                // Deregisters the panel row when the task ends, and
                // until then it is how the task talks to that row.
                let running_task = running_task;
                // Declared with the other guards so it drops before
                // them: the channel is gone before the session can be
                // claimed again.
                let mut child_steer = child_steer;
                let _slot = _guard; // hold the concurrency slot for the run
                // Nobody is waiting on a background task, so it stops the
                // moment it is told to — including part-way through the
                // revalidation, which may be a git call.
                let acquired = tokio::select! {
                    outcome = acquire_task_lease(
                        &workspace,
                        workspace_access,
                        inherited_lease,
                        cross_workspace_nested,
                        &parent_location,
                        &leased_location,
                        &task_cancel,
                        // A detached task has no tool row, but it has a
                        // panel row, and a mutable one queued behind
                        // another read there as working.
                        WaitAnnouncement::Panel(&running_task),
                        Some(Holder::Detached(format!(
                            "task \"{description}\" (task_id {session_id})"
                        ))),
                    ) => outcome,
                    () = task_cancel.cancelled() => LeaseOutcome::Cancelled,
                };
                let mut holder = None;
                let never_ran = match acquired {
                    LeaseOutcome::Acquired(lease, mark) => {
                        child_ctx.workspace_lease = Some(lease);
                        holder = mark;
                        None
                    }
                    LeaseOutcome::Cancelled => Some(TaskOutcome::Cancelled),
                    LeaseOutcome::Failed(failure) => Some(TaskOutcome::Failed(anyhow::anyhow!(
                        "{}",
                        failure.message()
                    ))),
                };
                if let Some(outcome) = never_ran {
                    // The hold hands back the messages its prompt took,
                    // so they are counted as the waiting ones they are.
                    drop(child_steer);
                    reserved.send(outcome.before_running(
                        &spawner.store,
                        &session_id,
                        &parent_session_id,
                        &description,
                        spawner.child_steers.pending(&session_id),
                    ));
                    return;
                }
                // Deliberately not a child of `task_cancel`: stopping the
                // task must go through the select below, which cancels
                // this token itself and then waits out the graceful
                // abort. Wiring it to `task_cancel` would let the turn
                // report a plain abort before the select noticed.
                let cancel = root_cancel.child_token();
                // The child's own token, not the root's: everything
                // inside this task stops when the task does. The turn
                // overrides this for the tools it runs, so the two agree
                // rather than one of them naming a token that outlives
                // the task.
                child_ctx.cancel = cancel.clone();
                child_ctx.detached_owner = Some(cancel.clone());
                let (tx, mut rx_evt) = loop_event_channel(LOOP_EVENT_CAPACITY);
                // Activity tracker: any event of this task's turn counts
                // as progress, and so does any event of a foreground
                // descendant, which touches the same heartbeat through
                // the context it inherits.
                let heartbeat = crate::tools::Heartbeat::new();
                child_ctx.heartbeat = Some(heartbeat.clone());
                // The panel reads the same clock the watchdog does, so
                // a task going quiet shows as `quiet 45s` long before
                // the watchdog decides it is dead.
                running_task.watch(&heartbeat);
                let watcher_heartbeat = heartbeat.clone();
                let watcher_activity = activity.clone();
                let watcher = tokio::spawn(async move {
                    while let Some(event) = rx_evt.recv().await {
                        watcher_heartbeat.touch();
                        watcher_activity.publish(event);
                    }
                });
                let stall_watch = async {
                    loop {
                        tokio::time::sleep(stall_timeout / 2).await;
                        if heartbeat.elapsed() >= stall_timeout {
                            return;
                        }
                    }
                };
                let mut turn = Box::pin(run_turn(
                    spawner.resolver.as_ref(),
                    &registry,
                    &spawner.store,
                    &session_id,
                    &prompt,
                    &[],
                    Some(&system_prompt),
                    spawner.loop_config.clone(),
                    tx,
                    cancel.clone(),
                    child_ctx,
                    // Not a user: the parent, whose message reaches this
                    // child at the same step boundary a root steer does.
                    Some(steer_rx),
                ));
                // Whichever way this ends, the child stops the same way a
                // foreground one does: the token is cancelled and the turn
                // is awaited, never dropped mid-flight.
                let stopped = async {
                    tokio::select! {
                        () = stall_watch => false,
                        () = task_cancel.cancelled() => true,
                    }
                };
                let (outcome, was_cancelled) = tokio::select! {
                    outcome = &mut turn => (Some(outcome), false),
                    was_cancelled = stopped => {
                        cancel.cancel();
                        let _ = tokio::time::timeout(BACKGROUND_ABORT_GRACE, &mut turn).await;
                        (None, was_cancelled)
                    }
                };
                // The event channel closes with the turn; the watcher ends
                // with it.
                drop(turn);
                // The lease went with the turn; its name goes too, before
                // the ending's file work, so nothing is refused behind a
                // holder that no longer holds anything.
                drop(holder);
                let _ = watcher.await;
                let outcome = match outcome {
                    Some(result) => TaskOutcome::from_turn(result),
                    None if was_cancelled => TaskOutcome::Cancelled,
                    None => TaskOutcome::Stalled,
                };
                // Only now is "started" decidable: a turn that declined
                // before appending its prompt destroyed nothing, and the
                // queue folded into that prompt goes back to waiting
                // when the steer hold drops. Every other ending appended
                // the prompt, queue included, so those messages are
                // delivered rather than waiting.
                if !outcome.turn_never_started() {
                    child_steer.started();
                }
                // Now, so what it gives back is counted below.
                drop(child_steer);
                activity.turn_done(outcome.activity());
                let queued = queued_note(spawner.child_steers.pending(&session_id));

                // Every ending's words come from `headline`; only the
                // clean finish is this branch's own, because it carries
                // the child's text and its resumable id.
                let notification = match outcome.headline(&description, stall_timeout) {
                    Some(body) => {
                        outcome.record(&spawner.store, &session_id, &body);
                        task_notification(
                            &parent_session_id,
                            &description,
                            &format!("{body}{queued}"),
                            true,
                        )
                    }
                    None => {
                        let text = final_assistant_text(&spawner.store, &session_id)
                            .unwrap_or_else(|| "(finished with no text)".into());
                        // The listing finds an undelivered result by the
                        // `task_id:` this names.
                        task_notification(
                            &parent_session_id,
                            &description,
                            &format!(
                                "Task \"{description}\" completed (task_id: {session_id}).\n<result>\n{text}\n</result>{queued}"
                            ),
                            false,
                        )
                    }
                };
                reserved.send(notification);
            });
            background_registry.tasks.push(BackgroundTask {
                id: registry_id,
                session_id: Some(returned_session_id.clone()),
                handle,
                cancel: background_cancel,
            });
            let _ = registered_tx.send(());
            if let Some(on_start) = on_start.take() {
                on_start();
            }
            // A mutable task in the parent's own checkout holds its
            // write lease until it reports: said here, so the parent
            // reads rather than edits meanwhile, instead of finding out
            // from a tool row that waits.
            let holds_checkout = if workspace_access == WorkspaceAccess::Mutating && same_workspace
            {
                " It holds this checkout's write lease until it reports, so your own edit, \
                 write, bash, service start and sudo calls are refused until then: read, glob \
                 and grep meanwhile, or give a mutable task a worktree of its own."
            } else {
                ""
            };
            return ToolOutput::text(format!(
                "Background task started (task_id: {returned_session_id}). Completion \
will trigger a separate follow-up turn. Do not sleep, poll, or check on it. Do not perform this \
task's scope yourself; continue only clearly disjoint work. With none left, call wait, or end \
your response with a one-line status, not an answer; the task keeps running either way.\
{holds_checkout}"
            ))
            .with_child_session(returned_session_id);
        }

        let waiting_notice = crate::tools::WorkspaceWaitNotice::from_context(ctx);
        // Marked whoever started it. It matters when the person did — a
        // message typed into its view — since then it sits outside any
        // step of the model's and is what a refused call is behind; a
        // model's own foreground task only ever has siblings that wait.
        let label = format!("task \"{}\" (task_id {session_id})", input.description);
        let holder = match in_step {
            InStep::Yes => Holder::InStep(label),
            InStep::No => Holder::Detached(label),
        };
        let (lease, _holder) = match acquire_task_lease(
            &child_workspace,
            workspace_access,
            inherited_lease,
            cross_workspace_nested,
            &ctx.location,
            &child_location,
            &ctx.cancel,
            waiting_notice
                .as_ref()
                .map_or(WaitAnnouncement::Silent, WaitAnnouncement::Row),
            Some(holder),
        )
        .await
        {
            LeaseOutcome::Acquired(lease, mark) => (lease, mark),
            LeaseOutcome::Cancelled => {
                return ToolOutput::error("subagent cancelled while waiting for workspace");
            }
            LeaseOutcome::Failed(failure) => return ToolOutput::error(failure.message()),
        };
        child_ctx.workspace_lease = Some(lease);
        if let Some(on_start) = on_start.take() {
            on_start();
        }
        let (tx, mut rx_evt) = loop_event_channel(LOOP_EVENT_CAPACITY);
        let activity = ActivityPublisher {
            tx: self.activity_tx.clone(),
            agent: agent.name.clone(),
            steers: self.child_steers.clone(),
            parent_session_id: ctx.session_id.clone(),
            parent_call_id,
            child_session_id: session_id.clone(),
        };
        let turn = run_turn(
            self.resolver.as_ref(),
            &registry,
            &self.store,
            &session_id,
            &prompt,
            &[],
            Some(&system_prompt),
            self.loop_config.clone(),
            tx,
            ctx.cancel.clone(),
            child_ctx,
            // Same channel as the background path: the parent of a
            // foreground task is blocked on it and cannot use it, but
            // the wiring is the child's, not the caller's.
            Some(steer_rx),
        );
        tokio::pin!(turn);
        let outcome = loop {
            tokio::select! {
                event = rx_evt.recv() => {
                    if let Some(event) = event {
                        // The caller is blocked on this child: its
                        // background ancestor's watchdog, if any, hears
                        // the child's progress as the caller's.
                        if let Some(heartbeat) = &ctx.heartbeat {
                            heartbeat.touch();
                        }
                        activity.publish(event);
                    }
                }
                outcome = &mut turn => break outcome,
            }
        };
        while let Ok(event) = rx_evt.try_recv() {
            activity.publish(event);
        }
        let outcome = TaskOutcome::from_turn(outcome);
        // Started means "the prompt was appended", and only the turn
        // knows: one that declined before appending marks its error, and
        // the queue folded into its prompt goes back to waiting instead
        // of vanishing with the prompt string.
        if !outcome.turn_never_started() {
            child_steer.started();
        }
        activity.turn_done(outcome.activity());

        // The same headline a detached task's parent would read: a
        // blocked caller and a notified one hear one verb set for one
        // ending. Only the clean finish differs, and only because the
        // caller wants the child's words, not a report about them.
        let output = match outcome.headline(&input.description, self.stall_timeout) {
            Some(body) => {
                outcome.record(&self.store, &session_id, &body);
                ToolOutput::error(body)
            }
            None => {
                let text = final_assistant_text(&self.store, &session_id)
                    .unwrap_or_else(|| "(finished with no text)".into());
                ToolOutput::text(text)
            }
        };
        // A default that could not be honoured says so: the schema
        // promised this task would be detached, and a silently blocking
        // call is exactly the surprise the promise was meant to remove.
        let output = match background_demoted {
            Some(note) => output.with_appended_text(&format!("\n\n({note})")),
            None => output,
        };
        // The session outlives the call, so name it: without this the
        // model cannot resume a task it just ran, and the resume path
        // tells it never to invent an id. A failed run is worth naming
        // too — an iteration-limited task is the one most worth
        // resuming.
        output
            .with_appended_text(&format!("\n\n(task_id: {session_id})"))
            .with_child_session(session_id.clone())
    }

    /// The one verb for talking to a task: steer it where it stands if
    /// its turn is running, resume it from its transcript if it has
    /// finished. The caller never has to know which case it is in, so
    /// every answer here says what actually happened to the message.
    pub async fn message_task(
        self: &Arc<Self>,
        input: TaskMessageInput,
        ctx: &ToolContext,
    ) -> ToolOutput {
        self.message_task_observed(input, ctx, None).await
    }

    /// The same send, saying what happened rather than telling a model
    /// about it: a UI that sent the message itself has its own words
    /// for "queued", "held" and "answered", and should not be reprinting
    /// a paragraph addressed to the model. A finished task resumes in
    /// the foreground here whatever the input says: the person waits
    /// for the answer in the UI, and a detached resume would hand it to
    /// the parent's model as a notification instead.
    pub async fn deliver_to_task(
        self: &Arc<Self>,
        input: TaskMessageInput,
        ctx: &ToolContext,
    ) -> TaskMessage {
        let input = TaskMessageInput {
            background: Some(false),
            ..input
        };
        self.message_task_outcome(input, ctx, None, InStep::No)
            .await
    }

    async fn message_task_observed(
        self: &Arc<Self>,
        input: TaskMessageInput,
        ctx: &ToolContext,
        on_start: Option<ToolStartObserver>,
    ) -> ToolOutput {
        self.message_task_outcome(input, ctx, on_start, InStep::Yes)
            .await
            .into_tool_output()
    }

    async fn message_task_outcome(
        self: &Arc<Self>,
        input: TaskMessageInput,
        ctx: &ToolContext,
        mut on_start: Option<ToolStartObserver>,
        in_step: InStep,
    ) -> TaskMessage {
        let text = input.message.trim().to_string();
        if text.is_empty() {
            return TaskMessage::Refused("message must not be empty".into());
        }
        let task_id = input.task_id.trim().to_string();
        // Only this session's own tasks: an id from somewhere else names
        // a conversation this session has no standing in, and the resume
        // path would refuse it a moment later anyway.
        let meta = match self.store.load(&task_id) {
            Ok(session) => match session.meta() {
                Some(meta) if meta.parent_id.as_deref() == Some(ctx.session_id.as_str()) => {
                    meta.clone()
                }
                Some(_) => {
                    return TaskMessage::Refused(format!(
                        "task {task_id:?} was not spawned by this session; the tasks tool lists the ones that were"
                    ));
                }
                None => {
                    return TaskMessage::Refused(format!("task {task_id:?} has no metadata"));
                }
            },
            Err(error) => {
                return TaskMessage::Refused(format!(
                    "unknown task {task_id:?}: {error}. Use an id from a task result, a \
                     task-notification, or the tasks tool, and never invent one."
                ));
            }
        };
        if self
            .running_tasks()
            .iter()
            .any(|task| task.session_id == task_id && !task.background)
        {
            return TaskMessage::Refused(format!(
                "task {task_id} is a foreground task of the turn you are in: you are blocked on \
                 its result, so nothing said now can reach it before it comes back. Messaging \
                 serves background tasks, which keep working while you do — the task tool's \
                 default, when you leave background out — and finished tasks, which this call \
                 resumes."
            ));
        }
        if self.child_steers.steer(&task_id, text.clone()) {
            if let Some(on_start) = on_start.take() {
                on_start();
            }
            return TaskMessage::Queued { task_id };
        }
        if self.session_is_active(&task_id) {
            // Running a turn this spawner did not start — a completion
            // routed to it. Holding the message is the undelivered rule:
            // that turn is already under way with its own prompt, so the
            // resume after it is the one that carries this.
            self.child_steers.queue(&task_id, text);
            if let Some(on_start) = on_start.take() {
                on_start();
            }
            return TaskMessage::Held { task_id };
        }
        // The one thing a resume needs that a message does not name: the
        // worktree the task ran in. It is in the task's own metadata, so
        // ask for it there rather than making the model know which case
        // it is in.
        let workspace = input
            .workspace
            .or_else(|| persisted_worktree(&meta, &ctx.location));
        // Parked in the pending queue before the resume is attempted,
        // exactly like the two branches above: the resume has many ways
        // to decline — the concurrency limit, the session-already-active
        // race the queue branch exists for, resume validation — and each
        // of them must leave the message waiting rather than gone. The
        // resume folds the queue into the prompt it appends and counts
        // it delivered only once the turn provably started, so the text
        // reaches the child exactly once on success and stays queued on
        // failure.
        self.child_steers.queue(&task_id, text.clone());
        let output = self
            .run_task_observed(
                TaskInput {
                    description: format!("message: {}", snippet(&text, TASK_MESSAGE_LABEL_CHARS)),
                    // Empty on purpose: the message rides the queue and
                    // heads the resume's prompt; naming it here too
                    // would deliver it twice.
                    prompt: String::new(),
                    subagent_type: meta.agent,
                    task_id: Some(task_id.clone()),
                    // The task tool's rule, passed through: omitted, the
                    // resume detaches and its answer is the completion
                    // notification; false is the caller blocked on it.
                    background: input.background,
                    workspace,
                    model: None,
                    reasoning: None,
                },
                ctx,
                on_start,
                in_step,
            )
            .await;
        // A declined resume is not a lost message, and the model must
        // not read it as one: whatever the refusal says — including the
        // concurrency cap's "wait for one to finish" — the message
        // itself is still parked and rides the child's next resume.
        let still_queued = output.is_error && self.child_steers.holds(&task_id, &text);
        let output = if still_queued {
            output.with_appended_text(
                "\n\n(Your message was not delivered by this call, but it is not lost: it is \
                 queued and read when this task is next resumed. When you resume it, send a \
                 short follow-up, not this text again.)",
            )
        } else {
            output
        };
        TaskMessage::Answered {
            task_id,
            output,
            still_queued,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn spawn_background_tool(
        self: &Arc<Self>,
        parent_session_id: String,
        kind: JobKind,
        description: String,
        timeout: std::time::Duration,
        future: ToolFuture,
        access: WorkspaceAccess,
        root_cancel: tokio_util::sync::CancellationToken,
    ) -> ToolOutput {
        let notification_permit = match self.notify_tx.clone().try_reserve_owned() {
            Ok(permit) => permit,
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                return ToolOutput::error(
                    "too many background tasks and jobs are running or waiting to be delivered: run this in the foreground, or end your turn and start it once a notification has arrived",
                );
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                return ToolOutput::error("background notification receiver is closed");
            }
        };
        let job_id = new_id();
        let notification_id = job_id.clone();
        // On the panel while it runs, like a task: a job that shows only
        // when it ends reads as a hang. Registered before the call
        // returns, as a task's row is, so the job is on the panel and in
        // `wait`'s view from the moment its start is reported.
        let running = self.register_running(RunningTask {
            session_id: parent_session_id.clone(),
            parent_session_id: String::new(),
            description: description.clone(),
            agent: JOB_AGENT.into(),
            job: Some(kind),
            background: true,
            delivering: false,
            started: std::time::Instant::now(),
            // Registry-owned: assigned and maintained by
            // `register_running` and the guard it hands back.
            row: 0,
            waiting: false,
            quiet: None,
            heartbeat: None,
        });
        // Same shape as a background task: a child of its owner's token
        // (`detached_owner`), so one token stands for "this job should
        // stop" — a stopped task takes its jobs with it, and
        // `abort_all`/`shutdown` can still cancel it alone.
        let background_cancel = root_cancel.child_token();
        let task_cancel = background_cancel.clone();
        let workspace = self.workspace.clone();
        {
            let mut background_registry = lock_unpoisoned(&self.background_tasks);
            if background_registry.closed {
                return ToolOutput::error("background runtime is shutting down");
            }
            background_registry
                .tasks
                .retain(|task| !task.handle.is_finished());
            let registry_id = new_id();
            let task_registry_id = registry_id.clone();
            let task_registry = self.background_tasks.clone();
            let (registered_tx, registered_rx) = tokio::sync::oneshot::channel();
            // Same guard as a background task's: a panicking job future
            // must surface as a failed-job notification, not as silence.
            let reserved = ReservedNotification::new(
                notification_permit,
                parent_session_id.clone(),
                description.clone(),
                self.outbox_dir.clone(),
            )
            .for_job(notification_id.clone());
            let handle = tokio::spawn(async move {
                let _running = running;
                if registered_rx.await.is_err() {
                    // Never admitted; the error was reported synchronously.
                    reserved.disarm();
                    return;
                }
                let reserved = reserved;
                let _background_task = BackgroundTaskGuard {
                    id: task_registry_id,
                    registry: task_registry,
                };
                let holder = format!("the background job \"{description}\"");
                let outcome = tokio::select! {
                    outcome = tokio::time::timeout(timeout, async move {
                        let permit = workspace.acquire(access).await;
                        let _holder = matches!(permit, WorkspacePermit::Mutating { .. })
                            .then(|| workspace.hold_as(holder));
                        future.await
                    }) => Some(outcome),
                    () = task_cancel.cancelled() => None,
                };
                let (text, is_error) = match outcome {
                    Some(Ok(output)) if output.is_error => (
                        format!(
                            "<tool-notification>\nBackground job {notification_id} (\"{description}\") failed.\n<result>\n{}\n</result>\n</tool-notification>",
                            output.content
                        ),
                        true,
                    ),
                    Some(Ok(output)) => (
                        format!(
                            "<tool-notification>\nBackground job {notification_id} (\"{description}\") completed.\n<result>\n{}\n</result>\n</tool-notification>",
                            output.content
                        ),
                        false,
                    ),
                    Some(Err(_)) => (
                        format!(
                            "<tool-notification>\nBackground job {notification_id} (\"{description}\") timed out after {} and was stopped.\n</tool-notification>",
                            crate::text::format_duration(timeout)
                        ),
                        true,
                    ),
                    None => (
                        format!(
                            "<tool-notification>\nBackground job {notification_id} (\"{description}\") was cancelled.\n</tool-notification>"
                        ),
                        true,
                    ),
                };
                reserved.send(Notification {
                    parent_session_id,
                    description,
                    text,
                    is_error,
                });
            });
            background_registry.tasks.push(BackgroundTask {
                id: registry_id,
                // A job has no session of its own; only cancel-all
                // reaches it, exactly as the panel's ⚙ row has
                // nothing to focus.
                session_id: None,
                handle,
                cancel: background_cancel,
            });
            let _ = registered_tx.send(());
        }
        ToolOutput::text(format!(
            "Background job {job_id} started. You will be notified when it completes. Do not poll or sleep; continue other work, or call wait, or end your response with a one-line status; the job keeps running either way."
        ))
    }

    /// Run a queued completion against its declared inactive parent. Child
    /// parents propagate one synthesized completion to their own parent.
    pub async fn route_notification(
        self: &Arc<Self>,
        notification: Notification,
        cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<RouteOutcome> {
        let parent = match self.store.load(&notification.parent_session_id) {
            Ok(parent) => parent,
            Err(_) => return Ok(RouteOutcome::Requeue(notification)),
        };
        // Already in the log? An outbox-recovered entry can race
        // another process delivering the same completion; the parent's
        // own log is the truth, and delivering twice is worse than the
        // load this check costs.
        if crate::delivery::is_delivered(
            &self.store,
            &notification.parent_session_id,
            &notification.text,
        ) {
            return Ok(RouteOutcome::Complete);
        }
        let meta = parent
            .meta()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("notification parent has no metadata"))?;
        // A live, steerable turn takes the result now — the way the
        // user's own message lands mid-turn — instead of a whole
        // resume queueing behind it. ChildSteers keeps the
        // exactly-once promise: a taken steer is appended by the
        // turn; an untaken one survives as pending and heads the next
        // resume's prompt. Whoever drives that turn also reports its
        // outcome upward, so nothing here owes the grandparent a word.
        if self
            .child_steers
            .steer(&notification.parent_session_id, notification.text.clone())
        {
            return Ok(RouteOutcome::Complete);
        }
        let agent = self
            .agents
            .iter()
            .find(|agent| agent.name == meta.agent)
            .ok_or_else(|| anyhow::anyhow!("unknown persisted agent {:?}", meta.agent))?;
        // Registered before any waiting starts: the panel and the
        // tasks tool read this registry, so a delivery queued behind
        // the session's current turn is visible mail, not an
        // invisible claim another turn trips over.
        let _running = self.register_running(RunningTask {
            session_id: notification.parent_session_id.clone(),
            parent_session_id: meta.parent_id.clone().unwrap_or_default(),
            description: notification.description.clone(),
            agent: meta.agent.clone(),
            job: None,
            background: true,
            delivering: true,
            started: std::time::Instant::now(),
            // Registry-owned: assigned and maintained by
            // `register_running` and the guard it hands back.
            row: 0,
            waiting: false,
            quiet: None,
            heartbeat: None,
        });
        let (workspace_location, depth) = match session_workspace_location(
            &self.store,
            &notification.parent_session_id,
            &self.workspace_location,
        )
        .await
        {
            Ok(location) => location,
            Err(error) => {
                return self.recorded_propagate(
                    &meta.session_id,
                    workspace_route_failure(&meta, notification, error),
                );
            }
        };
        let workspace = self.workspace.scoped(&workspace_location);
        let runtime = self.derived(workspace_location.clone(), workspace.clone(), depth);
        let _active_session = match self.claim_for_delivery(&notification, &cancel).await {
            ClaimOutcome::Claimed(claim) => claim,
            // A live turn took it: that turn reports its own outcome
            // upward, so nothing here owes the grandparent a word.
            ClaimOutcome::Steered => return Ok(RouteOutcome::Complete),
            ClaimOutcome::Busy => return Ok(RouteOutcome::Requeue(notification)),
        };
        let workspace_access = match agent.workspace_mode {
            AgentWorkspaceMode::Mutable => WorkspaceAccess::Mutating,
            AgentWorkspaceMode::ReadOnly => WorkspaceAccess::ReadOnly,
        };
        let registry = runtime.agent_registry(agent)?;
        let system_prompt = match self.agent_system_prompt(agent, workspace_location.cwd()) {
            Ok(prompt) => prompt,
            Err(error) => {
                return self.recorded_propagate(
                    &meta.session_id,
                    context_route_failure(&meta, notification, error),
                );
            }
        };
        let lease = tokio::select! {
            lease = workspace.acquire_lease(workspace_access) => lease,
            () = cancel.cancelled() => return Ok(RouteOutcome::Requeue(notification)),
            // Capped, because the claim above is held for the whole
            // wait: a mutable task holding the lease would otherwise
            // pin every delivery to this session, not just this one.
            () = tokio::time::sleep(ROUTE_LEASE_WAIT) => {
                return Ok(RouteOutcome::Requeue(notification));
            }
        };
        let _holder = (workspace_access == WorkspaceAccess::Mutating).then(|| {
            workspace.hold_as(format!(
                "task {} while it takes the result of \"{}\"",
                notification.parent_session_id, notification.description
            ))
        });
        // The lease may have been waited on for a while: re-derive the
        // workspace and make sure it is still the one that was resolved
        // before the wait.
        let (leased_location, leased_depth) = match session_workspace_location(
            &self.store,
            &notification.parent_session_id,
            &self.workspace_location,
        )
        .await
        {
            Ok(location) => location,
            Err(error) => {
                return self.recorded_propagate(
                    &meta.session_id,
                    workspace_route_failure(&meta, notification, error),
                );
            }
        };
        if leased_location != workspace_location || leased_depth != depth {
            return self.recorded_propagate(
                &meta.session_id,
                workspace_route_failure(
                    &meta,
                    notification,
                    anyhow::anyhow!("workspace changed while waiting for its lease"),
                ),
            );
        }
        // A routed notification is a turn of this session's own, and it
        // belongs to no call in the parent: nothing up there asked for
        // it, this session's background task did. Without a boundary of
        // its own, replay reads the whole turn — tools, edits and all —
        // as a continuation of whichever invocation happened to be
        // last, and draws it under a task row that never ran it. The
        // id is deliberately one no tool call can have, so a surface
        // that slices by invocation stops here instead of attributing.
        //
        // Only for a session with a parent: a parentless one is never
        // sliced by invocation, and marking it would cost it the
        // checkpoint a root turn takes (`agent::turn`).
        let notification_call_id = || {
            meta.parent_id
                .as_ref()
                .map(|_| format!("notification:{}", new_id()))
        };
        let mut lock_attempts = 0;
        let outcome = loop {
            if cancel.is_cancelled() {
                return Ok(RouteOutcome::Requeue(notification));
            }
            // A routed notification is a resume of this session, so it
            // carries what the session never read — the parent's
            // messages go in ahead of the completion that woke it. The
            // hold is per attempt: an attempt that never appended
            // anything puts them back when it drops.
            let mut queued = self.child_steers.adopt(&notification.parent_session_id);
            let text = queued.prompt(&notification.text);
            let result = run_turn(
                self.resolver.as_ref(),
                &registry,
                &self.store,
                &notification.parent_session_id,
                &text,
                &[],
                Some(&system_prompt),
                self.loop_config.clone(),
                discarded_event_sender(),
                cancel.clone(),
                ToolContext {
                    cwd: workspace_location.cwd().to_path_buf(),
                    session_id: notification.parent_session_id.clone(),
                    // Minted per attempt: two attempts that both got
                    // as far as appending would otherwise write one id
                    // twice, and a slicer keys on the first of those.
                    call_id: notification_call_id(),
                    depth,
                    subagent: Some(runtime.clone()),
                    workspace: workspace.clone(),
                    location: workspace_location.clone(),
                    workspace_lease: Some(lease.clone()),
                    workspace_ancestry: vec![workspace_location.id().clone()],
                    cancel: cancel.clone(),
                    output_tail: None,
                    // Set by the turn below from the notified session's
                    // own model.
                    vision: false,
                    // A routed notification is a fresh view of the
                    // session: nothing has been read on this context yet.
                    seen_files: crate::tools::SeenFiles::default(),
                    // The spawner is built from configuration, not from a
                    // tool context, so this path has no state directory
                    // to spill into and truncates as it always did.
                    spill_dir: None,
                    // A routed notification turn runs under no background
                    // watchdog.
                    heartbeat: None,
                    secrets: self.secrets.clone(),
                    // The notified session is the seat that delegated
                    // the work; a result arriving does not move it to
                    // another room.
                    withheld: self.withheld.clone(),
                    // Set by the turn from its steers; this one has none.
                    steers: None,
                    // What this delivery starts stops with it.
                    detached_owner: Some(cancel.clone()),
                },
                // No live channel: this turn is not the parent's to
                // steer, so a message that arrives while it runs waits
                // for the resume after it.
                None,
            )
            .await;
            match result {
                Err(error) if never_started_would_block(&error) => {
                    // The lease belongs to another turn and nothing was
                    // appended: wait for it briefly, then hand the
                    // notification back like every other transient
                    // failure here rather than spinning on the lock.
                    lock_attempts += 1;
                    if lock_attempts >= NOTIFICATION_LOCK_ATTEMPTS {
                        return Ok(RouteOutcome::Requeue(notification));
                    }
                    tokio::select! {
                        () = cancel.cancelled() => return Ok(RouteOutcome::Requeue(notification)),
                        () = tokio::time::sleep(NOTIFICATION_LOCK_RETRY) => {}
                    }
                }
                result => {
                    // Only a turn that appended its prompt delivered the
                    // queue folded into it; one that declined before the
                    // append marks its error, and the hold's drop puts
                    // the messages back — the restore this loop's adopt
                    // promises. WouldBlock took the arm above, so the
                    // retry loop never reaches here.
                    let never_started = matches!(
                        &result,
                        Err(error)
                            if error.downcast_ref::<crate::agent::TurnNeverStarted>().is_some()
                    );
                    if !never_started {
                        queued.started();
                    }
                    break result;
                }
            }
        };
        let Some(grandparent_id) = meta.parent_id else {
            return match outcome {
                Ok(TurnOutcome::Completed) => Ok(RouteOutcome::Complete),
                // Terminal, not requeued: the turn appended the
                // notification before it was stopped, so the session
                // has the text and replaying it would deliver it
                // twice. Only the word changes — a turn someone
                // stopped was cancelled, and the driver says so.
                Ok(TurnOutcome::Aborted) => {
                    Err(anyhow::anyhow!("notification parent turn was cancelled"))
                }
                Ok(TurnOutcome::MaxIterations) => Err(anyhow::anyhow!(
                    "notification parent reached its iteration limit"
                )),
                Err(error) => Err(error),
            };
        };
        let (status, text, is_error) = match nested_hop_ending(&outcome) {
            Some((status, text, is_error)) => (status, text, is_error),
            None => (
                "completed",
                final_assistant_text(&self.store, &notification.parent_session_id)
                    .unwrap_or_else(|| "(finished with no text)".into()),
                false,
            ),
        };
        // Named after the task the hop is about: the grandparent's row
        // leads with what finished, never with "Nested task" alone.
        let text = format!(
            "<task-notification>\nNested task \"{}\" {status}.\n<result>\n{text}\n</result>\n</task-notification>",
            notification.description
        );
        self.recorded_propagate(
            &meta.session_id,
            Ok(RouteOutcome::Propagate(Notification {
                parent_session_id: grandparent_id,
                description: notification.description,
                text,
                is_error,
            })),
        )
    }

    /// A propagated hop is synthesized here and exists nowhere else —
    /// unlike a task completion, no permit guard recorded it at birth.
    /// Every `Propagate` and `Replace` leaves through this, so the
    /// memory it rides to the next hop is never the only copy.
    ///
    /// It is also where a propagated hop says what still waits in
    /// `hop_session`: a routed turn has no live channel, so a message
    /// the parent sent while it ran is held for the next resume, and
    /// this report is the parent's only word of that. Every caller runs
    /// after the turn's hold has dropped, so the count is whole. A
    /// replacement says nothing: its session could not be restored, so
    /// "delivered when next resumed" is a promise the next resume may
    /// break — and a replayed replacement must match the first byte for
    /// byte, which a count taken twice need not.
    fn recorded_propagate(
        &self,
        hop_session: &str,
        outcome: anyhow::Result<RouteOutcome>,
    ) -> anyhow::Result<RouteOutcome> {
        let outcome = match outcome {
            Ok(RouteOutcome::Propagate(notification)) => {
                let note = hop_queued_note(self.child_steers.pending(hop_session), hop_session);
                Ok(RouteOutcome::Propagate(with_queued_note(
                    notification,
                    &note,
                )))
            }
            other => other,
        };
        let propagated = match &outcome {
            Ok(RouteOutcome::Propagate(notification)) => Some(notification),
            // Recorded, never retired here: the retire of the origin is
            // the driver's, and it must not happen before the
            // replacement is durably somewhere, or a crash in between
            // loses the child's work outright.
            Ok(RouteOutcome::Replace(notification)) => Some(notification),
            _ => None,
        };
        if let (Some(dir), Some(notification)) = (self.outbox_dir.as_deref(), propagated) {
            crate::outbox::record(dir, notification);
        }
        outcome
    }

    /// Whether a session is running right now — a claimed session is one
    /// a turn is driving, and the listing says so rather than showing a
    /// stale "last word".
    pub fn session_is_active(&self, session_id: &str) -> bool {
        lock_unpoisoned(&self.active_sessions).contains(session_id)
    }

    /// The subagents working right now, oldest first, across every
    /// depth — one shared registry, so a nested task shows up too.
    pub fn running_tasks(&self) -> Vec<RunningTask> {
        lock_unpoisoned(&self.running_tasks)
            .iter()
            .map(|task| {
                // Read at snapshot time, not at registration: a
                // stored duration would be as old as the row.
                let mut snapshot = task.clone();
                snapshot.quiet = task
                    .heartbeat
                    .as_ref()
                    .map(crate::tools::Heartbeat::elapsed);
                snapshot
            })
            .collect()
    }

    fn register_running(&self, mut task: RunningTask) -> RunningTaskGuard {
        static NEXT_ROW: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        task.row = NEXT_ROW.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let row = task.row;
        lock_unpoisoned(&self.running_tasks).push(task);
        RunningTaskGuard {
            row,
            registry: self.running_tasks.clone(),
        }
    }

    fn claim_session(&self, session_id: &str) -> Option<ActiveSessionGuard> {
        let mut active = lock_unpoisoned(&self.active_sessions);
        if !active.insert(session_id.to_string()) {
            return None;
        }
        Some(ActiveSessionGuard {
            session_id: session_id.to_string(),
            active: self.active_sessions.clone(),
            changed: self.active_sessions_changed.clone(),
        })
    }

    /// Take the session for this delivery's own turn, or get the result
    /// into a turn that can take it live. Either is a delivery;
    /// `Busy` is the only answer that owes the user anything.
    ///
    /// The rounds are what make the wait both bounded and self-healing.
    /// Waiting on the claim alone waited out whatever held it, however
    /// long that was; giving up after one round would hand an ordinary
    /// child turn back to the user. Between rounds the result is
    /// re-offered as a steer, so a session that has moved on to a
    /// steerable turn takes it at its next step.
    async fn claim_for_delivery(
        &self,
        notification: &Notification,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> ClaimOutcome {
        for _ in 0..NOTIFICATION_CLAIM_ROUNDS {
            if let Some(claim) = self
                .wait_for_session_claim(&notification.parent_session_id, cancel)
                .await
            {
                return ClaimOutcome::Claimed(claim);
            }
            if cancel.is_cancelled() {
                return ClaimOutcome::Busy;
            }
            // Same promise as the steer at the top of `route_notification`:
            // ChildSteers keeps it exactly-once, so an accepted steer is
            // a delivery and whoever drives that turn reports it upward.
            if self
                .child_steers
                .steer(&notification.parent_session_id, notification.text.clone())
            {
                return ClaimOutcome::Steered;
            }
        }
        ClaimOutcome::Busy
    }

    /// One round of the above: claim the session, or give up when the
    /// round's deadline passes. `None` also covers cancellation — the
    /// caller tells them apart by asking the token.
    async fn wait_for_session_claim(
        &self,
        session_id: &str,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Option<ActiveSessionGuard> {
        let mut changed = self.active_sessions_changed.subscribe();
        let deadline = tokio::time::Instant::now() + self.claim_wait;
        loop {
            if let Some(claim) = self.claim_session(session_id) {
                return Some(claim);
            }
            tokio::select! {
                result = changed.changed() => result.ok()?,
                () = cancel.cancelled() => return None,
                () = tokio::time::sleep_until(deadline) => return None,
            }
        }
    }
}

async fn session_workspace_location(
    store: &SessionStore,
    session_id: &str,
    root: &crate::tools::WorkspaceLocation,
) -> anyhow::Result<(crate::tools::WorkspaceLocation, usize)> {
    let mut chain = Vec::new();
    let mut current = session_id.to_string();
    let mut visited = std::collections::HashSet::new();
    loop {
        if !visited.insert(current.clone()) {
            return Err(SessionAncestryError(format!("session parent cycle at {current}")).into());
        }
        let session = store.load(&current).map_err(|error| {
            SessionAncestryError(format!("loading session {current:?}: {error}"))
        })?;
        let meta = session
            .meta()
            .cloned()
            .ok_or_else(|| SessionAncestryError(format!("session {current} has no metadata")))?;
        let parent = meta.parent_id.clone();
        chain.push(meta);
        let Some(parent) = parent else {
            break;
        };
        current = parent;
    }

    let mut location = root.clone();
    for meta in chain.iter().rev() {
        let Some(persisted) = &meta.workspace else {
            continue;
        };
        if persisted == &location {
            continue;
        }
        if persisted.id() == location.id() {
            anyhow::bail!(
                "session {:?} workspace does not match its parent's cwd and isolation",
                meta.session_id
            );
        }
        let restored = crate::tools::WorkspaceLocation::revalidate(&location, persisted).await?;
        if restored != *persisted {
            anyhow::bail!(
                "session {:?} workspace metadata does not match its canonical location",
                meta.session_id
            );
        }
        location = restored;
    }
    Ok((location, chain.len().saturating_sub(1)))
}

/// The router's retry case: see [`TurnNeverStarted::writer_held`].
fn never_started_would_block(error: &anyhow::Error) -> bool {
    crate::agent::TurnNeverStarted::writer_held(error)
}

fn workspace_route_failure(
    meta: &SessionMeta,
    notification: Notification,
    error: anyhow::Error,
) -> anyhow::Result<RouteOutcome> {
    if error.downcast_ref::<SessionAncestryError>().is_some() {
        return Ok(RouteOutcome::Requeue(notification));
    }
    let Some(grandparent_id) = &meta.parent_id else {
        return Err(anyhow::anyhow!(
            "notification workspace routing failed: {error:#}"
        ));
    };
    Ok(replacing(
        grandparent_id,
        notification,
        "its workspace could not be restored",
        &format!("{error:#}"),
    ))
}

fn context_route_failure(
    meta: &SessionMeta,
    notification: Notification,
    error: anyhow::Error,
) -> anyhow::Result<RouteOutcome> {
    let Some(grandparent_id) = &meta.parent_id else {
        return Err(error).context("loading routed subagent context");
    };
    Ok(replacing(
        grandparent_id,
        notification,
        "its context could not be loaded",
        &format!("{error:#}"),
    ))
}

/// The note that replaces an origin nothing could deliver: why the
/// target is unreachable *and* the origin's own text, because the work
/// in it is a finished child's only word and the plumbing error is the
/// least interesting half of the news. See [`RouteOutcome::Replace`] for
/// the retire this obliges.
///
/// The shape is the producers' shape and not a new one: `Task "{d}"
/// failed: …` on the first line, everything else in one `<result>`.
/// Every surface that collapses a notification into a row parses
/// exactly that (`session_view::normalize_task_notification`), so a
/// sentence of our own invention would show the reader a paragraph of
/// raw envelope where a headline belongs.
fn replacing(
    grandparent_id: &str,
    origin: Notification,
    reason: &str,
    error: &str,
) -> RouteOutcome {
    let text = format!(
        "<task-notification>\nNested task \"{}\" failed: {reason} — its result follows.\n<result>\n{error}\n\n{}\n</result>\n</task-notification>",
        origin.description,
        unwrapped(&origin.text)
    );
    RouteOutcome::Replace(Notification {
        parent_session_id: grandparent_id.to_string(),
        description: origin.description,
        text,
        is_error: true,
    })
}

/// A notification's text without its own envelope. The replacement
/// supplies one, and a `<task-notification>` nested inside another is
/// a wall of tags in every surface that unwraps exactly one.
fn unwrapped(text: &str) -> &str {
    for tag in ["task-notification", "tool-notification"] {
        if let Some(inner) = text
            .strip_prefix(&format!("<{tag}>\n"))
            .and_then(|inner| inner.strip_suffix(&format!("\n</{tag}>")))
        {
            return inner;
        }
    }
    text
}

#[derive(Debug, thiserror::Error)]
#[error("invalid session ancestry: {0}")]
struct SessionAncestryError(String);

async fn revalidate_after_lease(
    parent: &crate::tools::WorkspaceLocation,
    location: &crate::tools::WorkspaceLocation,
) -> anyhow::Result<crate::tools::WorkspaceLocation> {
    if location == parent {
        return Ok(location.clone());
    }
    match location.isolation() {
        crate::tools::WorkspaceIsolation::Shared => Ok(location.clone()),
        crate::tools::WorkspaceIsolation::GitWorktree { .. } => {
            crate::tools::WorkspaceLocation::revalidate(parent, location).await
        }
    }
}

/// A child's reasoning variant together with where it came from. A
/// rejected variant has to name its source: reporting an explicit
/// `reasoning` input as "inherited" sends the model looking for a
/// setting it just passed in, and reporting an inherited one as an input
/// sends it looking for one it never wrote.
struct TaskVariant {
    id: String,
    source: TaskVariantSource,
}

enum TaskVariantSource {
    /// The task call's own `reasoning` field.
    Input,
    /// The parent session's current variant, carried into the child.
    Parent,
}

impl TaskVariant {
    fn from_input(id: String) -> Self {
        Self {
            id,
            source: TaskVariantSource::Input,
        }
    }

    fn from_parent(id: String) -> Self {
        Self {
            id,
            source: TaskVariantSource::Parent,
        }
    }

    /// Why this variant cannot be used, naming both the source the model
    /// can act on and the variants the model does take — a rejection
    /// without the list is one it can only fix by guessing.
    fn rejection(&self, model: &str) -> String {
        let source = match self.source {
            TaskVariantSource::Input => "from the task's reasoning input",
            TaskVariantSource::Parent => "inherited from parent",
        };
        let options = match crate::model::find(model) {
            Some(info) if !info.variants().is_empty() => format!(
                "this model's variants: {}",
                info.variants()
                    .iter()
                    .map(|variant| variant.id)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Some(_) => "this model takes no reasoning variants; omit reasoning".to_string(),
            None => "this model is not in the catalog; omit reasoning".to_string(),
        };
        format!(
            "unsupported reasoning {:?} for {model} ({source}) — {options}",
            self.id
        )
    }
}

/// The isolated workspace a task was persisted with, as the task tool
/// takes it — `None` when the task ran in the caller's own checkout and
/// no workspace has to be named at all.
fn persisted_worktree(
    meta: &SessionMeta,
    parent: &crate::tools::WorkspaceLocation,
) -> Option<TaskWorkspaceInput> {
    let persisted = meta.workspace.as_ref()?;
    if persisted == parent {
        return None;
    }
    match persisted.isolation() {
        crate::tools::WorkspaceIsolation::Shared => None,
        crate::tools::WorkspaceIsolation::GitWorktree { .. } => Some(TaskWorkspaceInput {
            cwd: persisted.cwd().to_path_buf(),
            isolation: TaskWorkspaceIsolation::GitWorktree,
        }),
    }
}

/// A task's workspace lease, or why the task never started.
enum LeaseOutcome {
    /// With the holder's mark, when one was asked for and the lease is
    /// the run's own.
    Acquired(
        Arc<crate::tools::WorkspaceLease>,
        Option<crate::tools::HolderMark>,
    ),
    Cancelled,
    Failed(LeaseFailure),
}

enum LeaseFailure {
    /// The target workspace is held by another task right now.
    Busy,
    /// What the lease covers is no longer the workspace that was
    /// validated; `Some` when deciding that failed outright.
    Changed(Option<anyhow::Error>),
}

impl LeaseFailure {
    /// Why the task could not start, in one wording for every caller.
    fn message(&self) -> String {
        match self {
            Self::Busy => "target workspace is busy; retry after the current task finishes".into(),
            Self::Changed(None) => "workspace changed while waiting for its lease".into(),
            Self::Changed(Some(error)) => {
                format!("workspace changed while waiting for its lease: {error:#}")
            }
        }
    }
}

/// Where a task says it is waiting for the workspace. A foreground one
/// writes into the tool row its blocked caller is watching; a detached
/// one has no row of its own, so it marks its entry on the agents
/// panel — which used to show it as plainly running, indistinguishable
/// from a task doing work. Either way nothing is said unless the lease
/// is actually contended.
enum WaitAnnouncement<'a> {
    /// The blocked caller's tool row.
    Row(&'a crate::tools::WorkspaceWaitNotice),
    /// The task's own row on the agents panel.
    Panel(&'a RunningTaskGuard),
    /// Nobody is watching this wait.
    Silent,
}

impl WaitAnnouncement<'_> {
    fn announce(&self) {
        match self {
            Self::Row(notice) => crate::tools::WorkspaceWaitNotice::announce(Some(notice)),
            Self::Panel(guard) => guard.set_waiting(true),
            Self::Silent => {}
        }
    }

    /// The lease landed: the panel marker comes back off. A tool row's
    /// notice is a line in a log and stays where it was written.
    fn settled(&self) {
        if let Self::Panel(guard) = self {
            guard.set_waiting(false);
        }
    }
}

/// Take the lease a child will run under, then confirm its workspace
/// survived the wait. An inherited lease already covers the child; a
/// nested task reaching into another workspace only tries for it, since
/// blocking there is how two tasks deadlock on each other's checkouts.
///
/// Both task paths funnel through here so the checks stay in step; they
/// differ only in how they phrase the outcome. `cancel` ends the wait
/// for the lease; a caller that also wants the revalidation abandoned
/// races this whole call against its own token. `waiting` says where a
/// real wait is announced — see [`WaitAnnouncement`].
#[allow(clippy::too_many_arguments)] // one funnel, one set of checks
async fn acquire_task_lease(
    workspace: &crate::tools::WorkspaceScheduler,
    access: WorkspaceAccess,
    inherited: Option<Arc<crate::tools::WorkspaceLease>>,
    cross_workspace_nested: bool,
    parent_location: &crate::tools::WorkspaceLocation,
    location: &crate::tools::WorkspaceLocation,
    cancel: &tokio_util::sync::CancellationToken,
    waiting: WaitAnnouncement<'_>,
    holder: Option<Holder>,
) -> LeaseOutcome {
    // A lease this run holds as its own, not one it runs inside of: only
    // then is it the one a refused call is behind.
    let owned = inherited.is_none() && access == WorkspaceAccess::Mutating;
    let lease = match inherited {
        Some(lease) => lease,
        None if cross_workspace_nested => match workspace.try_acquire_lease(access) {
            Some(lease) => lease,
            None => return LeaseOutcome::Failed(LeaseFailure::Busy),
        },
        None => match workspace.try_acquire_lease(access) {
            Some(lease) => lease,
            // The same wait the tool executor announces, from the task
            // side: a mutable task queued behind another one must name
            // itself, not sit on a silent row.
            None => {
                waiting.announce();
                let lease = tokio::select! {
                    lease = workspace.acquire_lease(access) => lease,
                    () = cancel.cancelled() => {
                        waiting.settled();
                        return LeaseOutcome::Cancelled;
                    }
                };
                waiting.settled();
                lease
            }
        },
    };
    // Marked the moment the lease is taken, before the revalidation below
    // (a git call): a foreground task checking in that window must see a
    // detached holder, or it waits out the whole run.
    let mark = holder.filter(|_| owned).map(|holder| match holder {
        Holder::Detached(label) => workspace.hold_as(label),
        Holder::InStep(label) => workspace.hold_in_step_as(label),
    });
    match revalidate_after_lease(parent_location, location).await {
        Ok(revalidated) if &revalidated == location => LeaseOutcome::Acquired(lease, mark),
        Ok(_) => LeaseOutcome::Failed(LeaseFailure::Changed(None)),
        Err(error) => LeaseOutcome::Failed(LeaseFailure::Changed(Some(error))),
    }
}

/// Who a run's lease is held by, in words, and whether that run holds a
/// step of the model's (see [`crate::tools::WorkspaceScheduler::hold_as`]).
enum Holder {
    Detached(String),
    InStep(String),
}

/// How a child's turn ended, in the terms both task paths share. The
/// foreground path phrases it as a tool result and the background one as
/// a notification; neither re-derives it.
enum TaskOutcome {
    Completed,
    Aborted,
    MaxIterations,
    Failed(anyhow::Error),
    /// Background only: a cancellation token stopped the run.
    Cancelled,
    /// Background only: the stall watchdog fired.
    Stalled,
}

impl TaskOutcome {
    fn from_turn(result: anyhow::Result<TurnOutcome>) -> Self {
        match result {
            Ok(TurnOutcome::Completed) => Self::Completed,
            Ok(TurnOutcome::Aborted) => Self::Aborted,
            Ok(TurnOutcome::MaxIterations) => Self::MaxIterations,
            Err(error) => Self::Failed(error),
        }
    }

    /// The one headline an ending gets, wherever it is read: the tool
    /// result a foreground caller sees and the notification a
    /// background parent sees say the same words for the same ending.
    /// `None` for a clean finish, whose headline is the child's own
    /// final text.
    ///
    /// The verb set is fixed here and nowhere else: *cancelled* for a
    /// stop someone asked for (an aborted turn is one),
    /// *failed* for everything the task did to itself, *stalled* for
    /// the watchdog. `task` for the what, in every one of them.
    fn headline(&self, description: &str, stall_timeout: std::time::Duration) -> Option<String> {
        Some(match self {
            Self::Completed => return None,
            // A turn aborts only when its token is cancelled: one event,
            // one word, whichever path saw it.
            Self::Aborted | Self::Cancelled => format!("Task \"{description}\" was cancelled."),
            Self::MaxIterations => {
                format!("Task \"{description}\" failed: it reached its iteration limit.")
            }
            Self::Failed(error) => format!("Task \"{description}\" failed: {error:#}"),
            Self::Stalled => format!(
                "Task \"{description}\" stalled: no progress for {}s. It has been stopped.",
                stall_timeout.as_secs()
            ),
        })
    }

    /// The ending the child's own log records, in the listing's verb
    /// set. `None` for a clean finish, whose record is the answer.
    fn ending(&self) -> Option<TurnEnding> {
        Some(match self {
            Self::Completed => return None,
            Self::Aborted | Self::Cancelled => TurnEnding::Cancelled,
            Self::MaxIterations | Self::Failed(_) => TurnEnding::Failed,
            Self::Stalled => TurnEnding::Stalled,
        })
    }

    /// Write this ending into the task's log under the headline its
    /// parent was told. Whether the run got as far as appending its
    /// prompt matters: see [`record_ending`].
    fn record(&self, store: &SessionStore, session_id: &str, headline: &str) {
        if let Some(ending) = self.ending() {
            record_ending(
                store,
                session_id,
                ending,
                headline,
                !self.turn_never_started(),
            );
        }
    }

    /// The notification for a run that never got as far as its turn —
    /// the lease wait was cancelled or refused — with the ending put on
    /// the log first. No run means no watchdog, so the stall timeout is
    /// unused; and it is the same headline the post-turn ending would
    /// have had, because it asks the same function.
    fn before_running(
        &self,
        store: &SessionStore,
        session_id: &str,
        parent_session_id: &str,
        description: &str,
        queued: usize,
    ) -> Notification {
        let body = self
            .headline(description, std::time::Duration::ZERO)
            .expect("a run that never started did not finish");
        if let Some(ending) = self.ending() {
            record_ending(store, session_id, ending, &body, false);
        }
        task_notification(
            parent_session_id,
            description,
            &format!("{body}{}", queued_note(queued)),
            true,
        )
    }

    /// What the terminal activity event carries: anything that is not a
    /// clean finish or an iteration limit reads as an abort.
    fn activity(&self) -> TurnOutcome {
        match self {
            Self::Completed => TurnOutcome::Completed,
            Self::MaxIterations => TurnOutcome::MaxIterations,
            _ => TurnOutcome::Aborted,
        }
    }

    /// Whether the turn provably appended nothing — the marker `run_turn`
    /// puts on every failure ahead of its prompt append. The caller's
    /// steer hold restores its queue on this outcome instead of counting
    /// the folded-in messages as delivered.
    fn turn_never_started(&self) -> bool {
        matches!(
            self,
            Self::Failed(error)
                if error.downcast_ref::<crate::agent::TurnNeverStarted>().is_some()
        )
    }
}

/// Fans a child's loop events out to whoever is watching this
/// delegation, tagged with the call that started it.
#[derive(Clone)]
struct ActivityPublisher {
    tx: tokio::sync::broadcast::Sender<SubagentActivity>,
    agent: String,
    /// The same events say when the child took a message, so this is
    /// where a delivered one stops counting as pending.
    steers: ChildSteers,
    parent_session_id: String,
    parent_call_id: String,
    child_session_id: String,
}

impl ActivityPublisher {
    fn publish(&self, event: LoopEvent) {
        if let LoopEvent::Steered { text, .. } = &event {
            self.steers.delivered(&self.child_session_id, text);
        }
        let _ = self.tx.send(SubagentActivity {
            parent_session_id: self.parent_session_id.clone(),
            parent_call_id: self.parent_call_id.clone(),
            child_session_id: self.child_session_id.clone(),
            agent: self.agent.clone(),
            event,
        });
    }

    fn turn_done(&self, outcome: TurnOutcome) {
        self.publish(LoopEvent::TurnDone { outcome });
    }
}

/// What a detached task's notification adds while messages to it are
/// still parked — sent after its turn ended, or taken by a resume that
/// never started. The foreground path says the same in its result; a
/// model told nothing reads the message as lost and sends it again.
fn queued_note(pending: usize) -> String {
    queued_note_to(pending, "this task")
}

/// The same line for a hop, which reports on a task it names by
/// description while the waiting messages were sent to the session the
/// hop ran in — "this task" would point at the wrong one.
fn hop_queued_note(pending: usize, hop_session: &str) -> String {
    queued_note_to(pending, &format!("task {hop_session}"))
}

fn queued_note_to(pending: usize, task: &str) -> String {
    match pending {
        0 => String::new(),
        1 => format!(
            "\n\n(A message to {task} is still queued, not lost: it is read when that task is \
             next resumed — a task_message to it, or a task call with its task_id. Do not send \
             it again.)"
        ),
        n => format!(
            "\n\n({n} messages to {task} are still queued, not lost: they are read when that \
             task is next resumed — a task_message to it, or a task call with its task_id. Do \
             not send them again.)"
        ),
    }
}

/// A built notification with [`queued_note`] put inside its envelope,
/// where the parent loop unwraps it along with the rest.
fn with_queued_note(mut notification: Notification, note: &str) -> Notification {
    const CLOSE: &str = "\n</task-notification>";
    if !note.is_empty() {
        notification.text = match notification.text.strip_suffix(CLOSE) {
            Some(body) => format!("{body}{note}{CLOSE}"),
            None => format!("{}{note}", notification.text),
        };
    }
    notification
}

/// Whether a foreground run holds a step of the model's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InStep {
    Yes,
    No,
}

/// A background task's word to its parent, in the envelope the parent
/// loop unwraps.
fn task_notification(
    parent_session_id: &str,
    description: &str,
    body: &str,
    is_error: bool,
) -> Notification {
    Notification {
        parent_session_id: parent_session_id.to_string(),
        description: description.to_string(),
        text: format!("<task-notification>\n{body}\n</task-notification>"),
        is_error,
    }
}

/// Put a task's ending on its own log, so the transcript does not simply
/// stop where the parent's abort took it down and the `tasks` listing
/// can name the ending later. Best-effort: the notification is the
/// ending's report of record, and a log that cannot take the line is
/// no reason to withhold it.
///
/// A run that never `started` — cancelled or refused before its prompt
/// was appended — touched the log with nothing. On a fresh task that
/// still deserves the line, or the listing reads a Meta-only log as
/// `finished`; on a *resumed* task it must not have one, because the
/// log's last run is a real one, answer and all, and stamping this
/// ending after it would present that answer as the partial words of a
/// cancelled run. The log itself says which case this is.
fn record_ending(
    store: &SessionStore,
    session_id: &str,
    ending: TurnEnding,
    detail: &str,
    started: bool,
) {
    let Ok(writer) = store.acquire_writer(session_id) else {
        return;
    };
    let Ok(mut session) = writer.load() else {
        return;
    };
    let has_a_run = session
        .events()
        .iter()
        .any(|event| matches!(event, crate::session::SessionEvent::UserMessage { .. }));
    if !started && has_a_run {
        return;
    }
    let _ = session.append(crate::session::SessionEvent::TurnEnded {
        id: new_id(),
        ending,
        detail: detail.to_string(),
        ts: chrono::Utc::now(),
    });
}

/// The one way a background task reports that it was stopped — it is
/// reachable from the lease wait, the revalidation and the run itself.
/// How a delivery got hold of the session it is for.
enum ClaimOutcome {
    /// It is ours to resume: the guard holds the claim.
    Claimed(ActiveSessionGuard),
    /// A turn that was already running took the result as a steer —
    /// delivered, with nothing left to do here.
    Steered,
    /// Still busy after the whole budget, or the delivery was
    /// cancelled. The result goes back for a later attempt.
    Busy,
}

/// How a nested hop reports the parent turn it just ran to the
/// grandparent: the verb for the headline, the body, and whether it
/// reads as an error. `None` for a clean finish, whose body is the
/// parent's own final text.
///
/// A cancelled turn says *cancelled*. Reporting the user's own stop as
/// "Nested task X failed" is both the wrong verb and the wrong blame:
/// nothing about the grandchild failed.
fn nested_hop_ending(
    outcome: &anyhow::Result<TurnOutcome>,
) -> Option<(&'static str, String, bool)> {
    match outcome {
        Ok(TurnOutcome::Completed) => None,
        Ok(TurnOutcome::Aborted) => Some((
            "was cancelled",
            "Nested parent turn was cancelled.".to_string(),
            true,
        )),
        Ok(TurnOutcome::MaxIterations) => Some((
            "failed",
            "Nested parent turn reached its iteration limit.".to_string(),
            true,
        )),
        Err(error) => Some((
            "failed",
            format!("Nested parent turn failed: {error:#}"),
            true,
        )),
    }
}

/// Undo a session that was created moments ago but could not be
/// initialized. The id is about to be forgotten, so nothing of it may
/// stay on disk — its log, its lock and its replay index all go, which
/// is exactly what `delete` does under the lease it takes itself. The
/// caller must have dropped the session first.
fn rollback_created_session(store: &SessionStore, id: &str) {
    let _ = store.delete(id);
}

/// What the abnormal-death guard is standing over. The two endings
/// differ in more than wording: a task can be resumed by id, a job
/// cannot be resumed at all.
enum AbnormalWork {
    /// The task's session and the store its parent's messages wait in,
    /// once known, so a death can still say what is left waiting.
    Task {
        session: Option<(String, ChildSteers)>,
    },
    Job {
        job_id: String,
    },
}

/// A background task's reserved place in the notification channel, as a
/// guard that enforces the exactly-one invariant the completion pipeline
/// promises. Every expected ending calls `send` once; but a plain
/// `OwnedPermit` dropped by a panic quietly returns its capacity and the
/// parent waits forever for a task that is gone. The guard holds what an
/// abnormal ending needs to say — the parent and the description — and
/// its `Drop` says it: dropped without `send`, it publishes a synthesized
/// "ended abnormally" failure in the same `<task-notification>` envelope
/// every other failure uses.
///
/// The one ending that must stay silent is the registration handshake: a
/// task that was never admitted already reported its error synchronously
/// through the tool call, so that path `disarm`s the guard instead.
struct ReservedNotification {
    permit: Option<tokio::sync::mpsc::OwnedPermit<Notification>>,
    parent_session_id: String,
    description: String,
    /// What died, when the guard has to say so itself. A task has a
    /// session to resume; a job is a tool call with neither a session
    /// nor a task id, and telling the model to resume it invites an
    /// invented one.
    work: AbnormalWork,
    /// The durable copy is written before the channel send, so a
    /// notification either reaches the channel with a disk record behind
    /// it or reaches neither.
    outbox_dir: Option<std::path::PathBuf>,
}

impl ReservedNotification {
    fn new(
        permit: tokio::sync::mpsc::OwnedPermit<Notification>,
        parent_session_id: String,
        description: String,
        outbox_dir: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            permit: Some(permit),
            parent_session_id,
            description,
            outbox_dir,
            work: AbnormalWork::Task { session: None },
        }
    }

    /// The same reservation, knowing its task's session: an abnormal
    /// ending then says which of the parent's messages still wait.
    fn for_session(mut self, session_id: String, steers: ChildSteers) -> Self {
        self.work = AbnormalWork::Task {
            session: Some((session_id, steers)),
        };
        self
    }

    /// The same reservation, guarding a background *job*: a tool call
    /// running detached, named by its job id.
    fn for_job(mut self, job_id: String) -> Self {
        self.work = AbnormalWork::Job { job_id };
        self
    }

    /// Publish the task's one notification, consuming the reservation.
    fn send(mut self, notification: Notification) {
        self.deliver(notification);
    }

    /// Give the reservation up without a word — only for a task that was
    /// never admitted, whose failure the caller reported synchronously.
    fn disarm(mut self) {
        self.permit = None;
    }

    fn deliver(&mut self, notification: Notification) {
        let Some(permit) = self.permit.take() else {
            return;
        };
        if let Some(dir) = &self.outbox_dir {
            crate::outbox::record(dir, &notification);
        }
        permit.send(notification);
    }
}

impl Drop for ReservedNotification {
    fn drop(&mut self) {
        if self.permit.is_none() {
            return;
        }
        // Reached only when no explicit ending ran — a panic unwinding
        // the task body, or a future ending on a path that forgot to
        // report. Silence here is the parent waiting forever.
        let notification = match &self.work {
            // Once the task body has taken its steer hold over as a
            // local, the hold is declared after this guard and so has
            // already dropped, handing back what its prompt took. Before
            // that the count can only come up short, never long.
            AbnormalWork::Task { session } => task_notification(
                &self.parent_session_id,
                &self.description,
                &format!(
                    "Task \"{}\" ended abnormally{} without reporting a result — most \
                     likely a panic in the task. Its session log holds whatever it finished; \
                     task_message resumes it to continue, or treat it as failed.{}",
                    self.description,
                    // Every other ending names the id; this one has to
                    // as well, or "resume it" asks for one made up.
                    session
                        .as_ref()
                        .map_or(String::new(), |(id, _)| format!(" (task_id: {id})")),
                    queued_note(
                        session
                            .as_ref()
                            .map_or(0, |(id, steers)| steers.pending(id))
                    )
                ),
                true,
            ),
            // A job has no session and no task id, so it wears the
            // envelope its siblings wear and is offered nothing to
            // resume — the advice would be an invitation to make an id
            // up.
            AbnormalWork::Job { job_id } => Notification {
                parent_session_id: self.parent_session_id.clone(),
                description: self.description.clone(),
                text: format!(
                    "<tool-notification>\nBackground job {job_id} (\"{}\") ended abnormally \
                     without reporting a result — most likely a panic in the job. Treat it \
                     as failed; run it again if you still need it.\n</tool-notification>",
                    self.description
                ),
                is_error: true,
            },
        };
        self.deliver(notification);
    }
}

/// Test seam for [`final_assistant_text`]: the compaction-anchor rule
/// is a load-bearing detail of every completion notification, and the
/// integration path to exercise it (a full routed resume) costs far
/// more than it proves.
#[doc(hidden)]
pub fn final_assistant_text_for_test(store: &SessionStore, session_id: &str) -> Option<String> {
    final_assistant_text(store, session_id)
}

/// The events of a task's last run: everything after the last user
/// message OR the last compaction cut, whichever is later. A long child
/// that compacted mid-turn loads a window whose task prompt is gone —
/// the compaction summary stands in for it, and what follows the cut is
/// the turn's. Anchoring on user messages alone reported a finished
/// 13KB research report as "(finished with no text)". A log with
/// neither — a task stopped before its prompt was ever appended — is
/// all tail.
fn last_run(events: &[crate::session::SessionEvent]) -> &[crate::session::SessionEvent] {
    let boundary = events
        .iter()
        .rposition(|event| {
            matches!(
                event,
                crate::session::SessionEvent::UserMessage { .. }
                    | crate::session::SessionEvent::Compaction { .. }
            )
        })
        .map_or(0, |boundary| boundary + 1);
    &events[boundary..]
}

/// How a run ended, when it did not finish. The ending its log records
/// first; for a log from before endings were written, an assistant
/// message the stream left `aborted` is the one trace there is. `None`
/// is a clean finish.
fn ending_of(run: &[crate::session::SessionEvent]) -> Option<TurnEnding> {
    run.iter()
        .rev()
        .find_map(|event| match event {
            crate::session::SessionEvent::TurnEnded { ending, .. } => Some(*ending),
            _ => None,
        })
        .or_else(|| {
            run.iter().rev().find_map(|event| match event {
                crate::session::SessionEvent::AssistantMessage { stop_reason, .. } => {
                    (stop_reason == "aborted").then_some(TurnEnding::Aborted)
                }
                _ => None,
            })
        })
}

/// The last thing the assistant said in a run: its answer, or where it
/// got to.
fn final_text_of(run: &[crate::session::SessionEvent]) -> Option<String> {
    run.iter().rev().find_map(|event| match event {
        crate::session::SessionEvent::AssistantMessage { content, .. } => {
            let text = content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string();
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    })
}

fn final_assistant_text(store: &SessionStore, session_id: &str) -> Option<String> {
    store
        .load(session_id)
        .ok()
        .and_then(|session| final_text_of(last_run(session.events())))
}

fn discarded_event_sender() -> LoopEventSender {
    let (sender, receiver) = loop_event_channel(LOOP_EVENT_CAPACITY);
    drop(receiver);
    sender
}

struct ActiveSessionGuard {
    session_id: String,
    active: Arc<Mutex<std::collections::HashSet<String>>>,
    changed: tokio::sync::watch::Sender<u64>,
}

impl Drop for ActiveSessionGuard {
    fn drop(&mut self) {
        lock_unpoisoned(&self.active).remove(&self.session_id);
        self.changed.send_modify(|version| {
            *version = version.wrapping_add(1);
        });
    }
}

struct BackgroundTaskGuard {
    id: String,
    registry: Arc<Mutex<BackgroundRegistry>>,
}

impl Drop for BackgroundTaskGuard {
    fn drop(&mut self) {
        lock_unpoisoned(&self.registry)
            .tasks
            .retain(|task| task.id != self.id);
    }
}

struct SlotGuard(Arc<AtomicUsize>);

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct TaskInput {
    pub description: String,
    pub prompt: String,
    pub subagent_type: String,
    #[serde(default, deserialize_with = "deserialize_optional_text")]
    pub task_id: Option<String>,
    /// Run detached; completion arrives as a notification. bash calls
    /// the same thing `run_in_background`, and a model that carries the
    /// name over must not have its `false` silently ignored.
    #[serde(default, alias = "run_in_background")]
    pub background: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_optional_workspace")]
    pub workspace: Option<TaskWorkspaceInput>,
    /// Model override for this task; omit to use the agent definition's
    /// model or inherit the parent's.
    #[serde(default, deserialize_with = "deserialize_optional_text")]
    pub model: Option<String>,
    /// Reasoning variant for the chosen model; omit for its default.
    #[serde(default, deserialize_with = "deserialize_optional_text")]
    pub reasoning: Option<String>,
}

/// Whether an optional field was left unfilled rather than answered.
///
/// GLM-5.3 writes the *string* "null" into optional task fields it means
/// to skip — `"task_id": "null"`, `"model": "null"`, `"reasoning":
/// "null"` — and taking that literally cost three failed round trips in
/// one session: resuming task "null", validating variant "null". Blank
/// strings arrive the same way. Both mean "not set", so every optional
/// field of `TaskInput` goes through here. Required fields deliberately
/// do not: a prompt of "null" is a prompt.
fn is_unfilled(text: &str) -> bool {
    let text = text.trim();
    text.is_empty() || text == "null"
}

fn deserialize_optional_text<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.filter(|text| !is_unfilled(text)))
}

/// The same quirk for the one optional field that is an object: a string
/// where a workspace belongs is the model writing "null". Anything else
/// is decoded as a workspace, so a genuinely malformed one still gets
/// its own error rather than a mismatched-variant one.
fn deserialize_optional_workspace<'de, D>(
    deserializer: D,
) -> Result<Option<TaskWorkspaceInput>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    if value.is_null() {
        return Ok(None);
    }
    if let Some(text) = value.as_str()
        && is_unfilled(text)
    {
        return Ok(None);
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(serde::de::Error::custom)
}

/// What became of a message handed to a task. The model's version of
/// each case is [`Self::into_tool_output`]; a driver that sent the
/// message on a person's behalf renders the case itself rather than
/// showing them a paragraph written for a model.
#[derive(Debug)]
pub enum TaskMessage {
    /// Steered into the task's running turn: it takes it at its next
    /// step.
    Queued { task_id: String },
    /// The task is busy with a turn this spawner did not start and has
    /// no live channel; the message waits for its next resume.
    Held { task_id: String },
    /// The task had finished, so it was resumed with the message as its
    /// prompt: this is what the resume returned — its answer, or its
    /// started note if it detached.
    Answered {
        task_id: String,
        output: ToolOutput,
        /// The resume declined and the message is still parked for the
        /// task's next one: a failure to the caller, but not a lost
        /// message, and a UI must not report it as one.
        still_queued: bool,
    },
    /// Nothing was sent, and why.
    Refused(String),
}

impl TaskMessage {
    pub fn into_tool_output(self) -> ToolOutput {
        match self {
            Self::Refused(why) => ToolOutput::error(why),
            // Not "delivered": the task takes it at its next step, and a
            // task that stops before reaching one leaves it waiting for
            // its resume. Saying more than that would have the model
            // count on a reading that may not happen.
            Self::Queued { task_id } => ToolOutput::text(format!(
                "Message queued for running task {task_id}; it reaches that task at its next \
                 step, and waits for the task's next resume if the task stops before then. Its \
                 answer comes back the way that task's answers always do — as its result or its \
                 completion notification — so do not wait for a reply here and do not repeat the \
                 message."
            ))
            .with_child_session(task_id),
            Self::Held { task_id } => ToolOutput::text(format!(
                "Task {task_id} is busy with a completion of its own and has no live channel; \
                 your message is held and read at its next resume. Do not send it again."
            ))
            .with_child_session(task_id),
            Self::Answered { output, .. } => output,
        }
    }
}

/// One message for one task, whatever that task is currently doing.
#[derive(Debug, Clone, Deserialize)]
pub struct TaskMessageInput {
    pub task_id: String,
    pub message: String,
    /// The task's own worktree, when it has one: a resume has to name it
    /// again, exactly as the task tool's `task_id` does.
    #[serde(default, deserialize_with = "deserialize_optional_workspace")]
    pub workspace: Option<TaskWorkspaceInput>,
    /// How a finished task's resume runs, with the task tool's default:
    /// omitted, detached. A running task is steered either way.
    #[serde(default, alias = "run_in_background")]
    pub background: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskWorkspaceInput {
    pub cwd: std::path::PathBuf,
    pub isolation: TaskWorkspaceIsolation,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskWorkspaceIsolation {
    GitWorktree,
}

/// The task tool is concurrency-safe within a provider step and manages a
/// child-lifetime workspace claim itself.
pub struct TaskTool {
    spawner: Arc<SubagentSpawner>,
}

impl TaskTool {
    pub fn new(spawner: Arc<SubagentSpawner>) -> Self {
        Self { spawner }
    }
}

impl Tool for TaskTool {
    fn name(&self) -> &'static str {
        "task"
    }

    fn description(&self) -> &'static str {
        "Delegate one bounded unit of work to an agent, and do not do that scope yourself; delegate independent reviews as tasks of their own. subagent_type fixes the task's tools: a read-only agent for inspection that needs no shell (several run at once), `review` for checks that must run tests, builds or git (it cannot edit), a mutable agent only when edits or other mutating tools are needed.\n\nA task runs in the background: the call returns at once, and the task's completion arrives as a notification that starts a follow-up turn, so you keep working and the person can reach you meanwhile. Never poll it; task_message steers it. Pass background false only when this turn's very next step needs the result — a review that gates a commit is the usual case. Foreground siblings called together run in parallel.\n\nA mutable task in your own checkout holds its write lease until it reports: mutable tasks there run one after another, each seeing the last one's edits, and meanwhile your own edit, write, bash, service start and sudo calls are refused (read, glob and grep are not). To run independent mutable tasks in parallel or to keep editing, give each a workspace — see that parameter."
    }

    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Concurrent
    }

    fn workspace_access(&self) -> WorkspaceAccess {
        WorkspaceAccess::Mutating
    }

    fn manages_workspace_access(&self) -> bool {
        true
    }

    fn input_schema(&self) -> serde_json::Value {
        let agents = self
            .spawner
            .agents()
            .iter()
            .map(|agent| agent.name.as_str())
            .collect::<Vec<_>>();
        let agent_guidance = self
            .spawner
            .agents()
            .iter()
            .map(|agent| {
                let mode = match agent.workspace_mode {
                    AgentWorkspaceMode::Mutable => "mutable",
                    AgentWorkspaceMode::ReadOnly => "read-only",
                };
                match &agent.tools {
                    Some(tools) => format!(
                        "{} ({mode}, tools: {}): {}",
                        agent.name,
                        tools.join("/"),
                        agent.description
                    ),
                    None => format!("{} ({mode}): {}", agent.name, agent.description),
                }
            })
            // One sentence per agent, whatever its description ends with.
            .map(|line| line.trim_end().trim_end_matches('.').to_string())
            .collect::<Vec<_>>()
            .join(". ");
        // Built from the configured agents, so the shape the model copies
        // names an agent that exists here rather than one from another
        // install; the scope is inspection, which every agent may do,
        // because which agents are mutable is per install.
        let example = self.spawner.agents().first().map(|agent| {
            serde_json::json!({
                "description": "trace the retry path",
                "prompt": "Find where the HTTP client retries in src/, and report the call sites and the backoff policy.",
                "subagent_type": agent.name,
            })
        });
        let overview = match example {
            Some(example) => format!("One agent, one bounded scope. Example: {example}."),
            None => "One agent, one bounded scope.".to_string(),
        };
        serde_json::json!({
            "type": "object",
            "description": overview,
            "properties": {
                "description": {"type": "string", "description": "Short task description (3-5 words)"},
                "prompt": {"type": "string", "description": "Full instructions for one bounded scope; meanwhile, do only disjoint work yourself."},
                "subagent_type": {
                    "type": "string",
                    "enum": agents,
                    "description": format!("The agent to run; it fixes the task's tools and whether it may write. {agent_guidance}.")
                },
                "task_id": {
                    "type": ["string", "null"],
                    "description": "A task to resume with its full context — prefer it for a follow-up on the same scope; omit to start a new one. Use an id from a task result, a notification or the tasks tool, and never invent one. A task that ran in its own worktree requires that same workspace passed again."
                },
                "model": {
                    "type": ["string", "null"],
                    "description": "Model override for this task (provider/model-id). Omit to inherit. Call the models tool to see options with pricing — prefer a cheap/fast model for mechanical work."
                },
                "reasoning": {
                    "type": ["string", "null"],
                    "description": "Reasoning variant for the chosen model (see the models tool). Omit for the model's default."
                },
                "background": {"type": "boolean", "description": "Omit to run detached. false blocks this turn until the task reports: only when your very next step needs the result, such as a review whose findings gate a commit. A mutable task passed false while another detached job holds its checkout is refused rather than left waiting."}
                ,"workspace": {
                    "type": ["object", "null"],
                    "description": format!("A Git worktree of this repository to run the task in; omit to use your own checkout, which is right for every read-only agent. Pass one to run mutable tasks in parallel, to keep editing while one runs, or to delegate a mutable task when you are yourself a subagent (your checkout is held for your whole run). ilar validates the path and never creates one: {WORKTREE_CORRECTION}. Outside a Git repository there is none. Tasks in separate worktrees run at the same time, and merging their results is yours."),
                    "properties": {
                        "cwd": {"type": "string"},
                        "isolation": {"type": "string", "enum": ["git_worktree"]}
                    },
                    "required": ["cwd", "isolation"],
                    "additionalProperties": false
                }
            },
            "required": ["description", "prompt", "subagent_type"]
        })
    }

    fn run(&self, input: serde_json::Value, ctx: ToolContext) -> ToolFuture {
        let spawner = self.spawner.clone();
        Box::pin(async move {
            let input: TaskInput = match crate::tools::parse_input(input, "task") {
                Ok(v) => v,
                Err(error) => return error,
            };
            spawner.run_task(input, &ctx).await
        })
    }

    fn run_observed(
        &self,
        input: serde_json::Value,
        ctx: ToolContext,
        on_start: ToolStartObserver,
    ) -> ToolFuture {
        let spawner = self.spawner.clone();
        Box::pin(async move {
            let input: TaskInput = match crate::tools::parse_input(input, "task") {
                Ok(v) => v,
                Err(error) => return error,
            };
            spawner
                .run_task_observed(input, &ctx, Some(on_start), InStep::Yes)
                .await
        })
    }
}

/// task_message: the one verb for talking to a task, running or
/// finished. Same concurrency and workspace declarations as the task
/// tool, because its finished branch *is* a task invocation.
pub struct TaskMessageTool {
    spawner: Arc<SubagentSpawner>,
}

impl TaskMessageTool {
    pub fn new(spawner: Arc<SubagentSpawner>) -> Self {
        Self { spawner }
    }
}

impl Tool for TaskMessageTool {
    fn name(&self) -> &'static str {
        "task_message"
    }

    fn description(&self) -> &'static str {
        "Send a message to a task you spawned, running or finished — you do not need to know which. A task that is still running reads it at its next step and keeps going; its answer comes as that task's own result or notification, never from this call. A task that has finished is resumed from its transcript with the message as its prompt, the way a task resume runs it (detached unless background is false). A message a task stopped before reading is not lost: it heads its next resume, and the tasks tool shows it waiting. A foreground task of the turn you are in cannot be messaged."
    }

    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Concurrent
    }

    fn workspace_access(&self) -> WorkspaceAccess {
        WorkspaceAccess::Mutating
    }

    fn manages_workspace_access(&self) -> bool {
        true
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "description": "One message for one task: read at its next step if it runs, a resume if it finished.",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "Task session UUID to talk to. Use an id reported by a task result, a task-notification, or the tasks tool, and never invent a value; it must be a task this session spawned."
                },
                "message": {
                    "type": "string",
                    "description": "What to tell the task — a correction, a constraint it should have had, or a follow-up question. Write it as if you could interrupt it, because for a running task that is exactly what this is."
                },
                "workspace": {
                    "type": ["object", "null"],
                    "description": "Omit: a finished task resumes in the worktree it ran in. Pass it only to name that same worktree again.",
                    "properties": {
                        "cwd": {"type": "string"},
                        "isolation": {"type": "string", "enum": ["git_worktree"]}
                    },
                    "required": ["cwd", "isolation"],
                    "additionalProperties": false
                },
                "background": {
                    "type": "boolean",
                    "description": "A finished task's resume, as the task tool's background: omitted, it detaches; false returns the answer here, and is refused while another detached job holds the task's checkout."
                }
            },
            "required": ["task_id", "message"]
        })
    }

    fn run(&self, input: serde_json::Value, ctx: ToolContext) -> ToolFuture {
        let spawner = self.spawner.clone();
        Box::pin(async move {
            match parse_task_message(input) {
                Ok(input) => spawner.message_task(input, &ctx).await,
                Err(error) => error,
            }
        })
    }

    /// Same deferral as the task tool: the resume branch is a task
    /// start, and it is announced when it starts rather than when the
    /// call is made.
    fn run_observed(
        &self,
        input: serde_json::Value,
        ctx: ToolContext,
        on_start: ToolStartObserver,
    ) -> ToolFuture {
        let spawner = self.spawner.clone();
        Box::pin(async move {
            match parse_task_message(input) {
                Ok(input) => {
                    spawner
                        .message_task_observed(input, &ctx, Some(on_start))
                        .await
                }
                Err(error) => error,
            }
        })
    }
}

fn parse_task_message(input: serde_json::Value) -> Result<TaskMessageInput, ToolOutput> {
    crate::tools::parse_input(input, "task_message")
}

/// How many tasks the listing reports, newest first. A long session
/// can accumulate dozens; the recent ones are the resumable ones.
const TASK_LISTING_LIMIT: usize = 20;
/// Display width of the message a resume is named by.
const TASK_MESSAGE_LABEL_CHARS: usize = 40;
/// Display width of a task's last reply in the listing.
const TASK_SNIPPET_CHARS: usize = 200;
/// How much of a result the parent has not received yet the listing
/// carries whole — the notification still brings the rest.
const TASK_RESULT_CHARS: usize = 8_000;

/// A result as written, up to `limit` characters, and a note about the
/// rest: unlike [`snippet`] this keeps the text's own lines, because it
/// is being read as the answer rather than glanced at.
fn whole_or_cut(text: &str, limit: usize) -> String {
    let total = text.chars().count();
    if total <= limit {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(limit).collect();
    cut.push_str(&format!(
        "\n  … ({} more characters; the notification carries the whole result)",
        total - limit
    ));
    cut
}

fn snippet(text: &str, limit: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > limit {
        let mut short: String = collapsed.chars().take(limit).collect();
        short.push('…');
        short
    } else {
        collapsed
    }
}

fn age_label(modified: std::time::SystemTime) -> String {
    let Ok(elapsed) = modified.elapsed() else {
        return "just now".to_string();
    };
    let seconds = elapsed.as_secs();
    match seconds {
        0..=59 => format!("{seconds}s ago"),
        60..=3599 => format!("{}m ago", seconds / 60),
        3600..=86_399 => format!("{}h ago", seconds / 3_600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

/// tasks: read-only listing of the subagent tasks this session has
/// spawned, so the model can see what it delegated and resume one with
/// the task tool instead of re-explaining a scope to a fresh agent.
pub struct TasksTool {
    spawner: Arc<SubagentSpawner>,
}

/// Waits inside the turn for this session's own background work. Models
/// trained on a harness with a blocking wait (Codex's `wait_agent`) act
/// out one they are denied: gpt-6-sol wrote sixty "still waiting" lines
/// and gpt-6-luna reasoned for minutes, 2026-09-23/24. Like Codex's, it
/// blocks until a task or job of this session finishes, a steer arrives
/// or the timeout passes, and returns only what happened: the result
/// itself arrives as a message, through the one delivery path that
/// already counts each result exactly once.
pub struct WaitTool {
    spawner: Arc<SubagentSpawner>,
}

impl WaitTool {
    pub fn new(spawner: Arc<SubagentSpawner>) -> Self {
        Self { spawner }
    }
}

/// Codex's bounds: a floor that keeps a wait called in a loop from
/// spinning, and a ceiling past which a turn held open is forgotten.
const WAIT_DEFAULT_MS: u64 = 30_000;
const WAIT_MIN_MS: u64 = 10_000;
const WAIT_MAX_MS: u64 = 3_600_000;
/// How long a finished job's result has to be steered into the turn
/// before the model is told it comes as the next turn instead: the TUI
/// steers at its next frame, the gateway, exec and serve never do.
const WAIT_STEER_GRACE: Duration = Duration::from_secs(2);
/// How often the running rows are looked at; they hold no waker.
const WAIT_POLL: Duration = Duration::from_millis(250);

fn wait_timeout(requested_ms: Option<u64>) -> Duration {
    Duration::from_millis(
        requested_ms
            .unwrap_or(WAIT_DEFAULT_MS)
            .clamp(WAIT_MIN_MS, WAIT_MAX_MS),
    )
}

/// The token a call's detached work hangs from. Background work belongs
/// to its session, not to the turn that started it: a root session's
/// turn token lives for one turn — Esc on a turn held open by `wait`
/// stopped the very build it waited for — so its work gets a token of
/// its own, stopped by cancel-all, its own cancel or shutdown. Inside a
/// background task or a delivery, the context names that task's token,
/// and a task stopped stops what it started; a foreground child names
/// its caller's.
pub(crate) fn detached_owner(ctx: &ToolContext) -> tokio_util::sync::CancellationToken {
    ctx.detached_owner.clone().unwrap_or_default()
}

/// What of `session_id`'s own detached work is running: its background
/// tasks, and its bash jobs, which are filed under the session itself.
fn own_background(spawner: &SubagentSpawner, session_id: &str) -> Vec<RunningTask> {
    spawner
        .running_tasks()
        .into_iter()
        .filter(|row| {
            row.background
                && !row.delivering
                && (row.parent_session_id == session_id
                    || (row.agent == JOB_AGENT && row.session_id == session_id))
        })
        .collect()
}

fn named<'a>(rows: impl IntoIterator<Item = &'a RunningTask>) -> String {
    rows.into_iter()
        .map(|row| format!("\"{}\"", row.description))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Which of `before` has finished since, named, and a sentence on what
/// is still running (empty when nothing is).
fn wait_progress(
    spawner: &SubagentSpawner,
    session_id: &str,
    before: &[RunningTask],
) -> (Option<String>, String) {
    let still = own_background(spawner, session_id);
    let finished: Vec<&RunningTask> = before
        .iter()
        .filter(|was| still.iter().all(|row| row.row != was.row))
        .collect();
    let finished = (!finished.is_empty()).then(|| named(finished));
    let running = if still.is_empty() {
        String::new()
    } else {
        format!(" Still running: {}.", named(&still))
    };
    (finished, running)
}

impl Tool for WaitTool {
    fn name(&self) -> &'static str {
        "wait"
    }

    fn description(&self) -> &'static str {
        "Wait for your background tasks and jobs: returns when one of them \
         finishes, when a new message arrives, or after timeout_ms \
         (default 30000, at least 10000). It does not return results — a \
         finished one arrives as a message, and the answer says whether in \
         this turn or as your next. With nothing else to do while they run, \
         call this rather than writing that you are waiting."
    }

    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Barrier
    }

    fn workspace_access(&self) -> WorkspaceAccess {
        WorkspaceAccess::None
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "timeout_ms": {"type": "integer", "description": "Longest wait (10000 to 3600000)"}
            }
        })
    }

    fn run(&self, input: serde_json::Value, ctx: ToolContext) -> ToolFuture {
        let spawner = self.spawner.clone();
        Box::pin(async move {
            let timeout = wait_timeout(input.get("timeout_ms").and_then(serde_json::Value::as_u64));
            let before = own_background(&spawner, &ctx.session_id);
            if before.is_empty() {
                return ToolOutput::text(
                    "Nothing of yours is running in the background, so there is nothing to \
                     wait for.",
                );
            }
            let steers = ctx.steers.clone();
            // Never, for a turn nothing can steer.
            let steered = || async {
                match &steers {
                    Some(signal) => signal.arrived().await,
                    None => std::future::pending().await,
                }
            };
            const FOLLOWS: &str = "Its result follows as the next message.";
            let deadline = tokio::time::Instant::now() + timeout;
            loop {
                let (finished, running) = wait_progress(&spawner, &ctx.session_id, &before);
                if let Some(finished) = finished {
                    // Esc ends the short parts of a wait too.
                    let follows = steers.is_some()
                        && tokio::select! {
                            () = ctx.cancel.cancelled() => {
                                return ToolOutput::error("wait: cancelled");
                            }
                            arrived = tokio::time::timeout(WAIT_STEER_GRACE, steered()) => {
                                arrived.is_ok()
                            }
                        };
                    let next = if follows {
                        FOLLOWS
                    } else {
                        "Its result arrives as your next turn: end your response now, with a \
                         one-line status."
                    };
                    return ToolOutput::text(format!("Finished: {finished}. {next}{running}"));
                }
                tokio::select! {
                    biased;
                    () = ctx.cancel.cancelled() => return ToolOutput::error("wait: cancelled"),
                    () = steered() => {
                        // A result steered in by the front end is sent
                        // just before its row goes: looked at again, it
                        // is the result, not a person.
                        tokio::select! {
                            () = ctx.cancel.cancelled() => {
                                return ToolOutput::error("wait: cancelled");
                            }
                            () = tokio::time::sleep(WAIT_POLL) => {}
                        }
                        let (finished, running) =
                            wait_progress(&spawner, &ctx.session_id, &before);
                        return ToolOutput::text(match finished {
                            Some(finished) => format!("Finished: {finished}. {FOLLOWS}{running}"),
                            None => format!(
                                "Interrupted: a new message arrived, and follows.{running}"
                            ),
                        });
                    }
                    () = tokio::time::sleep_until(deadline) => {
                        return ToolOutput::text(format!(
                            "Timed out after {}s.{running} Call wait again, or end your \
                             response: results also arrive as a new turn.",
                            timeout.as_secs()
                        ));
                    }
                    () = tokio::time::sleep(WAIT_POLL) => {}
                }
                // A child task that waits is working, not stalled.
                if let Some(heartbeat) = &ctx.heartbeat {
                    heartbeat.touch();
                }
            }
        })
    }
}

impl TasksTool {
    pub fn new(spawner: Arc<SubagentSpawner>) -> Self {
        Self { spawner }
    }
}

impl Tool for TasksTool {
    fn name(&self) -> &'static str {
        "tasks"
    }

    fn description(&self) -> &'static str {
        "List the subagent tasks this session has spawned: id, agent, \
         model, how it stands, how many of your messages it has not read \
         yet (pending), and what it said. A task is running, finished, \
         cancelled, failed or stalled (older logs also say aborted, the \
         same as cancelled); only a finished one has \
         an answer, shown as `result:` — a stopped task's last words are \
         shown as `partial:` and are not findings. A finished task's \
         result reaches you once, as a notification; `result not \
         delivered to you yet` means it is on its way and the listing \
         carries it (up to 8000 characters), so do not resume the task \
         to ask for it again. Pass an id to task_message to talk to one — a running \
         task is steered at its next step, a finished one is resumed \
         with its context intact — or back as the task tool's task_id to \
         ask a finished task a follow-up on the same scope."
    }

    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Concurrent
    }

    fn workspace_access(&self) -> WorkspaceAccess {
        WorkspaceAccess::None
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    fn run(&self, _input: serde_json::Value, ctx: ToolContext) -> ToolFuture {
        let spawner = self.spawner.clone();
        Box::pin(async move {
            let children = spawner.store.children_of(&ctx.session_id);
            if children.is_empty() {
                return ToolOutput::text("no tasks spawned from this session yet");
            }
            let total = children.len();
            // Results this session has been sent and not yet heard —
            // held by the driver, queued behind this very turn. A
            // completion names its task (`(task_id: …)`), which is how
            // one is matched to its row; a bare id would also match a
            // sibling's result that merely mentions this task.
            let undelivered = spawner.undelivered_results(&ctx.session_id);
            let children: Vec<_> = children.into_iter().take(TASK_LISTING_LIMIT).collect();
            // A listing reads up to twenty children's logs, and a big
            // child is a big log. Inline, that is a provider step
            // waiting on a runtime worker doing file I/O for what the
            // description calls a cheap read-only listing.
            let ids: Vec<String> = children.iter().map(|child| child.id.clone()).collect();
            let store = spawner.store.clone();
            // The parked-message count reads a file too, so it goes
            // with them rather than putting the I/O straight back.
            let steers = spawner.child_steers.clone();
            let loaded = crate::tools::blocking_scan(move |cancelled| {
                ids.iter()
                    .map(|id| (store.load_until(id, &cancelled).ok(), steers.pending(id)))
                    .collect::<Vec<_>>()
            })
            .await
            .unwrap_or_default();
            let mut lines = children
                .into_iter()
                .zip(
                    loaded
                        .into_iter()
                        .chain(std::iter::repeat_with(|| (None, 0))),
                )
                .map(|(child, (session, waiting_count))| {
                    let running = spawner.session_is_active(&child.id);
                    let delivering = spawner
                        .running_tasks()
                        .iter()
                        .any(|task| task.delivering && task.session_id == child.id);
                    let run = session
                        .as_ref()
                        .map_or(&[][..], |session| last_run(session.events()));
                    let ending = if running { None } else { ending_of(run) };
                    let marker = format!("(task_id: {})", child.id);
                    let result_waiting = ending.is_none()
                        && undelivered.iter().any(|notification| {
                            !notification.is_error && notification.text.contains(&marker)
                        });
                    let status = if delivering {
                        // Active, but not on the model's behalf: a
                        // background result is being delivered to it.
                        "running (receiving a task result)"
                    } else if running {
                        "running"
                    } else if let Some(ending) = ending {
                        ending.verb()
                    } else if result_waiting {
                        "finished · result not delivered to you yet"
                    } else {
                        "finished"
                    };
                    let prompt = child.title.as_deref().unwrap_or("(no prompt)");
                    let last = if running {
                        // Its final text is not final yet.
                        String::new()
                    } else {
                        match final_text_of(run) {
                            // A stopped task's last words are where it
                            // got to, not what it found.
                            Some(text) if ending.is_some() => {
                                format!("\n  partial: {}", snippet(&text, TASK_SNIPPET_CHARS))
                            }
                            // An answer the parent has not received is
                            // carried whole: the alternative is a second
                            // run of the task to ask for it again.
                            Some(text) if result_waiting => {
                                format!("\n  result: {}", whole_or_cut(&text, TASK_RESULT_CHARS))
                            }
                            Some(text) => {
                                format!("\n  result: {}", snippet(&text, TASK_SNIPPET_CHARS))
                            }
                            None => String::new(),
                        }
                    };
                    // What the parent said and the task has not read:
                    // in flight while it runs, waiting for its resume
                    // once it has stopped. Either way it is owed a
                    // reading, so the listing says so.
                    let waiting = match waiting_count {
                        0 => String::new(),
                        1 => " · 1 message pending".to_string(),
                        count => format!(" · {count} messages pending"),
                    };
                    format!(
                        "{} · {} · {} · {status}{waiting} · {}\n  task: {}{last}",
                        child.id,
                        child.agent,
                        child.model,
                        age_label(child.modified),
                        snippet(prompt, TASK_SNIPPET_CHARS),
                    )
                })
                .collect::<Vec<_>>();
            if total > TASK_LISTING_LIMIT {
                lines.push(format!(
                    "({} older tasks not shown)",
                    total - TASK_LISTING_LIMIT
                ));
            }
            ToolOutput::text(lines.join("\n"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wait_keeps_codexs_bounds() {
        assert_eq!(wait_timeout(None), Duration::from_secs(30));
        assert_eq!(wait_timeout(Some(5)), Duration::from_secs(10));
        assert_eq!(wait_timeout(Some(60_000)), Duration::from_secs(60));
        assert_eq!(wait_timeout(Some(u64::MAX)), Duration::from_secs(3600));
    }

    fn notification(description: &str) -> Notification {
        Notification {
            parent_session_id: "parent".into(),
            description: description.into(),
            text: description.into(),
            is_error: false,
        }
    }

    fn reserved(
        sender: &tokio::sync::mpsc::Sender<Notification>,
        description: &str,
    ) -> ReservedNotification {
        ReservedNotification::new(
            sender.clone().try_reserve_owned().unwrap(),
            "parent".into(),
            description.into(),
            None,
        )
    }

    /// One ending, one sentence — whoever reads it. A blocked caller
    /// used to hear "subagent aborted" for what a notified parent
    /// heard as `Task "X" was aborted.`; `headline` is the only place
    /// either wording lives now.
    #[test]
    fn every_ending_has_one_headline() {
        let stall = std::time::Duration::from_secs(600);
        let headline = |outcome: TaskOutcome| outcome.headline("survey the API", stall);

        // A clean finish has no headline: the child's own words are it.
        assert_eq!(headline(TaskOutcome::Completed), None);
        assert_eq!(
            headline(TaskOutcome::Aborted).as_deref(),
            Some("Task \"survey the API\" was cancelled.")
        );
        assert_eq!(
            headline(TaskOutcome::Cancelled).as_deref(),
            Some("Task \"survey the API\" was cancelled.")
        );
        assert_eq!(
            headline(TaskOutcome::MaxIterations).as_deref(),
            Some("Task \"survey the API\" failed: it reached its iteration limit.")
        );
        assert_eq!(
            headline(TaskOutcome::Failed(anyhow::anyhow!("no provider"))).as_deref(),
            Some("Task \"survey the API\" failed: no provider")
        );
        assert_eq!(
            headline(TaskOutcome::Stalled).as_deref(),
            Some("Task \"survey the API\" stalled: no progress for 600s. It has been stopped.")
        );
    }

    /// A run stopped before its turn says the same thing the post-turn
    /// ending would, and puts it on the log — unless the log already
    /// holds a real run. A resumed task whose resume was cancelled while
    /// it waited for the lease keeps its clean answer as its last run:
    /// an ending stamped after it would present that answer as the
    /// partial words of a cancelled one.
    #[test]
    fn an_ending_before_the_turn_never_overwrites_a_real_run() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let fresh = new_id();
        let resumed = new_id();
        for id in [&fresh, &resumed] {
            store
                .create(SessionMeta {
                    session_id: id.clone(),
                    parent_id: Some("parent".into()),
                    agent: "explore".into(),
                    model: "zai/glm-4.7".into(),
                    workspace: None,
                    cwd: None,
                })
                .unwrap();
        }
        let mut session = store.acquire_writer(&resumed).unwrap().load().unwrap();
        session
            .append(crate::session::SessionEvent::UserMessage {
                id: new_id(),
                text: "survey the API".into(),
                images: Vec::new(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(session);

        let recorded = |id: &str| ending_of(last_run(store.load(id).unwrap().events()));

        let notification =
            TaskOutcome::Cancelled.before_running(&store, &fresh, "parent", "survey the API", 0);
        assert!(
            notification
                .text
                .contains("Task \"survey the API\" was cancelled."),
            "{}",
            notification.text
        );
        assert!(
            !notification.text.contains("queued"),
            "{}",
            notification.text
        );
        assert_eq!(recorded(&fresh), Some(TurnEnding::Cancelled));

        let notification =
            TaskOutcome::Cancelled.before_running(&store, &resumed, "parent", "survey the API", 2);
        assert!(
            notification
                .text
                .contains("2 messages to this task are still queued"),
            "{}",
            notification.text
        );
        assert_eq!(recorded(&resumed), None, "the real run stands");
    }

    /// A nested hop whose parent turn the user stopped is a
    /// cancellation, not the grandchild's failure: the verb said
    /// "failed" for every unhappy ending, which blamed the child for
    /// the user's keypress.
    #[test]
    fn a_cancelled_nested_hop_says_cancelled() {
        let (status, text, is_error) =
            nested_hop_ending(&Ok(TurnOutcome::Aborted)).expect("an abort ends the hop");
        assert_eq!(status, "was cancelled");
        assert!(text.contains("cancelled"), "{text}");
        assert!(is_error);

        assert_eq!(
            nested_hop_ending(&Ok(TurnOutcome::MaxIterations))
                .expect("a limit ends the hop")
                .0,
            "failed"
        );
        assert_eq!(
            nested_hop_ending(&Err(anyhow::anyhow!("boom")))
                .expect("an error ends the hop")
                .0,
            "failed"
        );
        // A clean finish leaves the body to the parent's own last word.
        assert!(nested_hop_ending(&Ok(TurnOutcome::Completed)).is_none());
    }

    /// A background job is a tool call, not a task: no child session,
    /// no task id. The guard that speaks for it when it dies must not
    /// offer the model something to resume — the id it would need does
    /// not exist, and the schemas warn about exactly that invention.
    #[tokio::test]
    async fn a_dead_job_is_reported_as_a_job_with_nothing_to_resume() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let guard = reserved(&sender, "compile the world").for_job("job-7".into());

        drop(guard);

        let died = receiver.recv().await.expect("the death was reported");
        assert!(died.is_error, "{}", died.text);
        assert!(
            died.text.starts_with("<tool-notification>"),
            "{}",
            died.text
        );
        assert!(died.text.contains("Background job job-7"), "{}", died.text);
        assert!(
            !died.text.contains("task tool"),
            "a job was offered a resume it cannot have: {}",
            died.text
        );
    }

    #[tokio::test]
    async fn notification_capacity_is_reserved_before_background_admission() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let guard = reserved(&sender, "first");
        assert!(matches!(
            sender.clone().try_reserve_owned(),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_))
        ));

        guard.send(notification("first"));
        assert_eq!(receiver.recv().await.unwrap().description, "first");
        assert!(receiver.try_recv().is_err());
    }

    /// The exactly-one invariant against abnormal endings: a reservation
    /// dropped without its explicit send — a panic unwinding the task —
    /// reports the death instead of returning capacity in silence.
    #[tokio::test]
    async fn a_dropped_reservation_reports_an_abnormal_ending() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        drop(reserved(&sender, "survey the retries"));

        let published = receiver.recv().await.unwrap();
        assert!(published.is_error);
        assert_eq!(published.parent_session_id, "parent");
        assert_eq!(published.description, "survey the retries");
        assert!(
            published.text.contains("<task-notification>"),
            "{}",
            published.text
        );
        assert!(
            published.text.contains("ended abnormally"),
            "{}",
            published.text
        );
    }

    /// A task that died still owes its waiting messages a word: the
    /// parent was told "queued" when it sent them, and a death that says
    /// nothing reads as having taken them along.
    #[tokio::test]
    async fn a_dropped_reservation_says_the_messages_that_wait() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let steers = ChildSteers::default();
        steers.queue("child", "also check the retries".into());
        drop(reserved(&sender, "survey the retries").for_session("child".into(), steers));

        let published = receiver.recv().await.unwrap();
        assert!(
            published.text.contains("ended abnormally"),
            "{}",
            published.text
        );
        assert!(
            published
                .text
                .contains("A message to this task is still queued"),
            "{}",
            published.text
        );
        assert!(
            published.text.ends_with("</task-notification>"),
            "{}",
            published.text
        );
    }

    /// The one silent ending: a task the registry never admitted already
    /// reported its error synchronously, so the disarmed guard says
    /// nothing.
    #[tokio::test]
    async fn a_disarmed_reservation_stays_silent() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        reserved(&sender, "never admitted").disarm();
        assert!(receiver.try_recv().is_err());
    }

    /// A mutable task queued behind another one is the same wait a
    /// mutating tool hits, and it says the same sentence — silence there
    /// reads as a hang. A lease that is free is taken without a word.
    #[tokio::test]
    async fn a_task_waiting_for_the_workspace_names_itself_in_the_row() {
        let dir = tempfile::tempdir().unwrap();
        let location = crate::tools::WorkspaceLocation::shared(dir.path().to_path_buf());
        let workspace = crate::tools::WorkspaceScheduler::for_location(&location);
        let tails = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::<
            String,
            String,
        >::new()));
        let (wake, _wake_rx) = tokio::sync::mpsc::channel(4);
        let notice = |call_id: &str| {
            let mut ctx = crate::tools::ToolContext::root(dir.path().to_path_buf());
            ctx.call_id = Some(call_id.into());
            ctx.output_tail = Some(crate::tools::OutputTailSink::new(
                tails.clone(),
                wake.clone(),
            ));
            crate::tools::WorkspaceWaitNotice::from_context(&ctx)
        };
        let cancel = tokio_util::sync::CancellationToken::new();
        let acquire = |waiting: Option<crate::tools::WorkspaceWaitNotice>| {
            let workspace = workspace.clone();
            let location = location.clone();
            let cancel = cancel.clone();
            async move {
                acquire_task_lease(
                    &workspace,
                    WorkspaceAccess::Mutating,
                    None,
                    false,
                    &location,
                    &location,
                    &cancel,
                    waiting
                        .as_ref()
                        .map_or(WaitAnnouncement::Silent, WaitAnnouncement::Row),
                    None,
                )
                .await
            }
        };

        let free = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            acquire(notice("free-1")),
        )
        .await
        .expect("a free workspace should not block");
        assert!(matches!(free, LeaseOutcome::Acquired(..)));
        assert!(
            !tails.lock().unwrap().contains_key("free-1"),
            "announced a wait that never happened"
        );

        let blocked = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            acquire(notice("blocked-1")),
        )
        .await;
        assert!(blocked.is_err(), "the held lease did not block a writer");
        assert_eq!(
            tails.lock().unwrap().get("blocked-1").map(String::as_str),
            Some(crate::tools::WORKSPACE_WAIT_NOTICE)
        );
    }

    #[test]
    fn rolling_back_a_created_session_leaves_no_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let id = new_id();
        drop(
            store
                .create(SessionMeta {
                    session_id: id.clone(),
                    parent_id: Some("parent".into()),
                    agent: "explore".into(),
                    model: "zai/glm-4.7".into(),
                    workspace: None,
                    cwd: None,
                })
                .unwrap(),
        );

        rollback_created_session(&store, &id);

        let leftovers: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(leftovers.is_empty(), "rollback left {leftovers:?}");
    }

    /// `task_message` tells the model a parked message "waits and is
    /// delivered at that task's next resume". Process memory does not,
    /// so the parked list is mirrored: a second store over the same
    /// directory — which is what a restart is — finds them.
    #[test]
    fn a_parked_message_outlives_the_process_that_parked_it() {
        let dir = tempfile::tempdir().unwrap();
        let mirrored = || ChildSteers::default().with_dir(dir.path().join("steers"));

        let steers = mirrored();
        steers.queue("child", "first".into());
        steers.queue("child", "second".into());
        assert_eq!(steers.pending("child"), 2);

        // The process goes; the directory stays.
        drop(steers);
        let after = mirrored();
        assert_eq!(after.pending("child"), 2, "a restart lost them");
        let (_receiver, mut run) = after.open("child");
        assert_eq!(run.prompt("go on"), "first\n\nsecond\n\ngo on");

        // Taken by a run that started, so nothing is owed any more and
        // nothing is left on disk for the next process to find.
        run.started();
        drop(run);
        assert_eq!(after.pending("child"), 0);
        assert_eq!(mirrored().pending("child"), 0);
        assert!(
            !dir.path().join("steers").join("child.json").exists(),
            "a claimed queue leaves no file behind"
        );

        // The window the mirror exists for: a run has taken the queue
        // into its prompt but has not committed it, and the process
        // dies. What it took is still owed, so the next one finds it.
        let mid_turn = mirrored();
        mid_turn.queue("other", "say the thing".into());
        let (_receiver, run) = mid_turn.open("other");
        assert_eq!(run.prompt(""), "say the thing");
        assert_eq!(
            mirrored().pending("other"),
            1,
            "a crash mid-turn lost what the run had claimed"
        );
        // A message sent while that run is in flight is owed too.
        mid_turn.queue("other", "and this".into());
        assert_eq!(mirrored().pending("other"), 2);
        std::mem::forget(run);

        // Without a directory nothing is written, and nothing breaks.
        let plain = ChildSteers::default();
        plain.queue("child", "held in memory".into());
        assert_eq!(plain.pending("child"), 1);
        assert_eq!(ChildSteers::default().pending("child"), 0);
    }

    /// The same words queued twice are one message sent twice — a model
    /// unsure the first was kept — and the child would read both.
    #[test]
    fn a_message_queued_twice_is_read_once() {
        let steers = ChildSteers::default();
        steers.queue("child", "also the tests".into());
        steers.queue("child", "also the tests".into());
        steers.queue("child", "and the docs".into());
        assert_eq!(steers.pending("child"), 2);
    }

    /// The undelivered rule at its own level: a run that never started
    /// hands the queue back in order, and a run that did takes it.
    #[test]
    fn an_unstarted_run_hands_its_queue_back_in_order() {
        let steers = ChildSteers::default();
        steers.queue("child", "first".into());
        steers.queue("child", "second".into());

        {
            let (_receiver, run) = steers.open("child");
            assert_eq!(run.prompt("go on"), "first\n\nsecond\n\ngo on");
            assert_eq!(steers.pending("child"), 0, "the run holds them");
        }
        assert_eq!(steers.pending("child"), 2, "an unstarted run kept them");

        let (_receiver, mut run) = steers.open("child");
        run.started();
        drop(run);
        assert_eq!(steers.pending("child"), 0);
    }

    #[test]
    fn a_message_reaches_a_running_turn_and_stops_pending_once_taken() {
        let steers = ChildSteers::default();
        assert!(!steers.steer("child", "before any turn".into()));

        let (mut receiver, mut run) = steers.open("child");
        run.started();
        assert!(steers.steer("child", "mid-turn".into()));
        assert_eq!(receiver.try_recv().unwrap().text, "mid-turn");
        assert_eq!(steers.pending("child"), 1, "sent is not yet read");

        steers.delivered("child", "mid-turn");
        assert_eq!(steers.pending("child"), 0);
        drop(run);
        assert!(!steers.steer("child", "after the turn".into()));
    }

    /// A steer the turn ended before reading is the message the next run
    /// opens with — the root rule, one level down.
    #[test]
    fn a_steer_the_turn_never_read_heads_the_next_run() {
        let steers = ChildSteers::default();
        let (receiver, mut run) = steers.open("child");
        run.started();
        assert!(steers.steer("child", "look at the migration".into()));
        drop(receiver);
        drop(run);

        let (_receiver, next) = steers.open("child");
        assert_eq!(next.prompt("continue"), "look at the migration\n\ncontinue");
    }

    #[test]
    fn nested_context_failure_replaces_the_origin_for_the_grandparent() {
        let meta = SessionMeta {
            session_id: "parent".into(),
            parent_id: Some("grandparent".into()),
            agent: "build".into(),
            model: "zai/glm-4.7".into(),
            workspace: None,
            cwd: None,
        };

        let outcome = context_route_failure(
            &meta,
            notification("nested"),
            anyhow::anyhow!("bad AGENTS.md"),
        )
        .unwrap();

        // Replace, not Propagate: nothing took what was routed, so the
        // driver owes that entry a retire.
        let RouteOutcome::Replace(propagated) = outcome else {
            panic!("expected the routed notification to be replaced");
        };
        assert_eq!(propagated.parent_session_id, "grandparent");
        assert!(propagated.is_error);
        assert!(propagated.text.contains("bad AGENTS.md"));
        // The child's work climbs with the plumbing error, not instead
        // of it: only the error used to, and the result was lost.
        assert!(
            propagated.text.contains(&notification("nested").text),
            "{}",
            propagated.text
        );
    }

    /// The replacement wears the producers' shape, because every
    /// surface that collapses a notification into a row parses that
    /// shape and nothing else: `Task "{d}" failed: …` on the first
    /// line, one `<result>` holding the rest. An invented sentence
    /// showed the reader a paragraph and then raw envelope tags.
    #[test]
    fn a_replacement_wears_the_shape_every_surface_parses() {
        let meta = SessionMeta {
            session_id: "parent".into(),
            parent_id: Some("grandparent".into()),
            agent: "build".into(),
            model: "zai/glm-4.7".into(),
            workspace: None,
            cwd: None,
        };
        let origin = Notification {
            parent_session_id: "parent".into(),
            description: "review the hub package".into(),
            text: "<task-notification>\nNested task \"review the hub package\" completed.\n<result>\nthe hub package is fine\n</result>\n</task-notification>".into(),
            is_error: false,
        };

        let RouteOutcome::Replace(propagated) =
            workspace_route_failure(&meta, origin, anyhow::anyhow!("the worktree is gone"))
                .unwrap()
        else {
            panic!("expected the routed notification to be replaced");
        };

        let inner = propagated
            .text
            .strip_prefix("<task-notification>\n")
            .and_then(|inner| inner.strip_suffix("\n</task-notification>"))
            .expect("one envelope, the way the producers write it");
        let (first, body) = inner.split_once('\n').expect("a headline and a body");
        // The verb the row splits on, and the description ahead of it.
        assert_eq!(
            first,
            "Nested task \"review the hub package\" failed: its workspace could not be \
             restored — its result follows."
        );
        // One `<result>`, and the origin's own envelope unwrapped
        // inside it rather than nested.
        let body = body
            .strip_prefix("<result>\n")
            .and_then(|body| body.strip_suffix("\n</result>"))
            .expect("the whole body is one result");
        assert!(body.contains("the worktree is gone"), "{body}");
        assert!(body.contains("the hub package is fine"), "{body}");
        assert!(!body.contains("<task-notification>"), "{body}");
    }
}
