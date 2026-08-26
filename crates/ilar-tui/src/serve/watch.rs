//! The supervisor: one directory poller, one tailer per watched session.
//!
//! Everything here is polling, because P1 says FSEvents cannot see an
//! append made through a writer's long-held fd — see the design in
//! meta/issues/ilar-serve-reads-the-store.md. Two loops, different
//! costs:
//!
//! - **the directory poller** (1 Hz): one `read_dir` plus a `stat` per
//!   entry — 5 ms for a thousand sessions. It feeds the head cache,
//!   which is what makes the listing cheap: a cold `SessionStore::list`
//!   head-parses every file (178–577 ms, P8), so a head is re-read only
//!   when that session's `(len, mtime, inode)` moved.
//! - **a tailer** (250 ms, only while someone is subscribed): one
//!   [`SessionTail::poll`], fanned out over a [`broadcast`] channel, and
//!   retired ~30 s after the last subscriber leaves so a closed tab does
//!   not keep a session hot.
//!
//! **Subscriber handoff is a snapshot, not a replay.** A tailer primes
//! itself with one poll before its first handoff, and [`Watcher::subscribe`]
//! takes the folded view, the physical line it stands at, and a fresh
//! receiver in one critical section — the same section a tailer step
//! sends in. So every subscriber, the first or the fifth, starts from a
//! coherent point and then sees exactly the lines after it: no replay
//! burst to race, no gap, no duplicate. (The alternative, replaying the
//! channel from the tailer's open, would hand a 906-event session's
//! whole log to a subscriber that only wanted the live edge, and would
//! still need the snapshot for anyone joining after the burst.)
//!
//! A subscriber that cannot keep up is *told* — [`next_message`] turns
//! the channel's `Lagged` into [`TailUpdate::Resync`], because silently
//! dropping lines would leave a client's folded transcript wrong forever.
//!
//! Forward compatibility: a committed line this build cannot parse is
//! terminal for that session only. The store's own message (which says
//! "written by a newer ilar?" when the tag is the tell) is broadcast as
//! [`TailMessage::Failed`] for the subscriber to render, the tailer
//! retires, and every other session keeps streaming.
//!
//! IO here is synchronous. The two loops put their own reads on a
//! blocking thread, because both have a path that reads a whole session
//! file (a cold scan, a resync); [`Watcher::subscribe`] does not, and
//! its caller — an HTTP handler — is the one that must.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use ilar::session::{SessionEvent, SessionHead, SessionId, SessionStore, SessionTail, TailUpdate};
use tokio::sync::broadcast;

/// One directory scan per second: P2 measured a 1,090-entry scan at
/// 5 ms, and the listing only needs to notice a new session.
pub(crate) const DIRECTORY_POLL: Duration = Duration::from_millis(1_000);
/// P2's number: a 250 ms stat-poll caught every step at the moment it
/// landed.
pub(crate) const TAIL_POLL: Duration = Duration::from_millis(250);
/// How long a tailer outlives its last subscriber. A reload or a flaky
/// connection should find the tail still warm.
pub(crate) const IDLE_LINGER: Duration = Duration::from_secs(30);
/// Lines a subscriber may fall behind before it is told to resync. One
/// poll's updates are sent in a tight loop before any receiver runs, so
/// this is also a floor on lines-per-poll: a 250 ms window that commits
/// more than this makes every subscriber resync, however fast it is.
pub(crate) const BROADCAST_CAPACITY: usize = 256;
/// A session counts as live while its file was touched this recently.
/// Deliberately not a lock probe: acquiring the writer lease to ask
/// would make a read-only server take the one thing it promised not to.
pub(crate) const LIVE_WINDOW: Duration = Duration::from_secs(60);
/// Escape hatch for both intervals; `--poll-ms` outranks it.
pub(crate) const POLL_MS_ENV: &str = "ILAR_SERVE_POLL_MS";

/// The poll intervals, as values, so tests do not sleep for seconds and
/// do not race each other through a process-wide env var.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WatchConfig {
    pub(crate) directory_poll: Duration,
    pub(crate) tail_poll: Duration,
    pub(crate) idle_linger: Duration,
    pub(crate) capacity: usize,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            directory_poll: DIRECTORY_POLL,
            tail_poll: TAIL_POLL,
            idle_linger: IDLE_LINGER,
            capacity: BROADCAST_CAPACITY,
        }
    }
}

impl WatchConfig {
    /// Tune both loops from one number, keeping the shipped 1:4 ratio
    /// between a tail poll and a directory scan.
    pub(crate) fn with_poll_ms(poll_ms: u64) -> Self {
        let tail_poll = Duration::from_millis(poll_ms.max(1));
        Self {
            directory_poll: tail_poll * 4,
            tail_poll,
            ..Self::default()
        }
    }

    /// The flag wins over the environment, and an unparsable or zero
    /// setting is ignored rather than obeyed into a busy loop.
    pub(crate) fn resolve(explicit: Option<u64>, env: Option<&str>) -> Self {
        let requested = explicit
            .or_else(|| env.and_then(|value| value.trim().parse::<u64>().ok()))
            .filter(|poll_ms| *poll_ms > 0);
        requested.map_or_else(Self::default, Self::with_poll_ms)
    }

    pub(crate) fn from_env(explicit: Option<u64>) -> Self {
        Self::resolve(explicit, std::env::var(POLL_MS_ENV).ok().as_deref())
    }
}

/// What a tailer fans out to its subscribers.
// Same trade as `TailUpdate` itself: boxing to shrink the rare failure
// variant would cost an allocation on every line a session appends.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TailMessage {
    Update(TailUpdate),
    /// The tail stopped on a line this build cannot parse; the payload
    /// is the store's own diagnostic, verbatim.
    Failed(String),
}

/// Why a tail is over. Carried on a [`Subscription`] as well as on the
/// channel, so a subscriber that arrives after the end still learns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TailEnd {
    Deleted,
    Failed(String),
}

/// A starting point plus the stream that continues from it.
#[derive(Debug)]
pub(crate) struct Subscription {
    /// Physical lines already consumed. Every message that follows
    /// names a line greater than this one.
    pub(crate) line: usize,
    /// The folded canonical view at that line — what the client's
    /// two-line fold (`rewind` → truncate, else push) continues from.
    pub(crate) events: Vec<SessionEvent>,
    /// Set when the tail was already over at handoff.
    pub(crate) ended: Option<TailEnd>,
    pub(crate) receiver: broadcast::Receiver<TailMessage>,
}

/// The next message, with a lagged subscriber turned into the resync it
/// needs. `None` when the tailer is gone for good.
pub(crate) async fn next_message(
    receiver: &mut broadcast::Receiver<TailMessage>,
) -> Option<TailMessage> {
    match receiver.recv().await {
        Ok(message) => Some(message),
        // Lines were dropped for this receiver only. It cannot know
        // which, so the whole view is suspect — exactly what a resync
        // means everywhere else in this system.
        Err(broadcast::error::RecvError::Lagged(_)) => {
            Some(TailMessage::Update(TailUpdate::Resync))
        }
        Err(broadcast::error::RecvError::Closed) => None,
    }
}

/// A session file as the directory poller sees it. Cheap enough to take
/// every second; a change in any field means the head must be re-read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stat {
    len: u64,
    modified: Option<SystemTime>,
    inode: u64,
}

fn stat_of(metadata: &std::fs::Metadata) -> Stat {
    Stat {
        len: metadata.len(),
        modified: metadata.modified().ok(),
        inode: inode_of(metadata),
    }
}

#[cfg(unix)]
fn inode_of(metadata: &std::fs::Metadata) -> u64 {
    std::os::unix::fs::MetadataExt::ino(metadata)
}

#[cfg(not(unix))]
fn inode_of(_metadata: &std::fs::Metadata) -> u64 {
    0
}

/// One listing row: the head the cache holds, plus whether the file was
/// touched recently enough to call the session live.
#[derive(Debug, Clone)]
pub(crate) struct SessionEntry {
    pub(crate) head: SessionHead,
    pub(crate) live: bool,
}

fn entry_of(head: &SessionHead) -> SessionEntry {
    SessionEntry {
        head: head.clone(),
        // A clock that moved backwards leaves a file "in the future";
        // read that as live rather than as ancient.
        live: SystemTime::now()
            .duration_since(head.modified)
            .is_ok_and(|age| age < LIVE_WINDOW),
    }
}

/// A cached head. `head` is `None` for a file that is not a readable
/// session (a foreign file, a half-written first line, a log from a
/// newer ilar): the stat is still remembered so the failing read is not
/// repeated every second.
#[derive(Debug, Clone)]
struct Cached {
    stat: Stat,
    head: Option<SessionHead>,
}

#[derive(Debug)]
struct TailState {
    tail: SessionTail,
    ended: Option<TailEnd>,
}

#[derive(Debug)]
struct Tailer {
    sender: broadcast::Sender<TailMessage>,
    state: Mutex<TailState>,
}

impl Tailer {
    fn subscribers(&self) -> usize {
        self.sender.receiver_count()
    }

    /// A snapshot and a receiver, taken together. Sends happen under
    /// this same lock, so the pair can neither miss a line nor repeat
    /// one.
    fn handoff(&self) -> Subscription {
        let state = lock(&self.state);
        Subscription {
            line: state.tail.line(),
            events: state.tail.events().to_vec(),
            ended: state.ended.clone(),
            receiver: self.sender.subscribe(),
        }
    }

    /// One poll, fanned out. `false` means the tail is over.
    fn step(&self) -> bool {
        let mut state = lock(&self.state);
        if state.ended.is_some() {
            return false;
        }
        match state.tail.poll() {
            Ok(updates) => {
                for update in updates {
                    if matches!(update, TailUpdate::Deleted) {
                        state.ended = Some(TailEnd::Deleted);
                    }
                    // Load-bearing: the send happens *inside* the state
                    // lock that `handoff` takes. Moving it out to shorten
                    // the critical section would silently reintroduce the
                    // gap-or-duplicate window a handoff cannot see.
                    let _ = self.sender.send(TailMessage::Update(update));
                }
                state.ended.is_none()
            }
            Err(error) => {
                // The store's text already explains itself, including
                // the "written by a newer ilar?" case. Rewording it here
                // would only lose the line number.
                let message = error.to_string();
                state.ended = Some(TailEnd::Failed(message.clone()));
                let _ = self.sender.send(TailMessage::Failed(message));
                false
            }
        }
    }
}

/// Take a lock, ignoring poisoning. A panic under one of these locks
/// would otherwise be permanent: the tailer task would die without
/// retiring itself, leaving an entry in the map whose every future
/// subscriber panics an HTTP handler. The state a panic could leave
/// behind is a stale line number, and the next poll fixes that.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct Inner {
    store: SessionStore,
    root: PathBuf,
    config: WatchConfig,
    cache: Mutex<HashMap<String, Cached>>,
    tailers: Mutex<HashMap<String, Arc<Tailer>>>,
    head_reads: AtomicU64,
}

/// The store as `ilar serve` watches it: a warm listing and a tail per
/// subscribed session. Cheap to clone — every clone shares one cache.
#[derive(Clone)]
pub(crate) struct Watcher {
    inner: Arc<Inner>,
}

impl Watcher {
    pub(crate) fn new(root: PathBuf, config: WatchConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                store: SessionStore::new(root.clone()),
                root,
                config,
                cache: Mutex::new(HashMap::new()),
                tailers: Mutex::new(HashMap::new()),
                head_reads: AtomicU64::new(0),
            }),
        }
    }

    pub(crate) fn store(&self) -> &SessionStore {
        &self.inner.store
    }

    /// One directory pass: notice new and deleted session files, and
    /// re-read the head of anything whose stat moved.
    pub(crate) fn refresh(&self) {
        self.inner.refresh();
    }

    /// The 1 Hz loop. It holds a weak handle, so dropping the last
    /// `Watcher` ends it.
    pub(crate) fn spawn_poller(&self) {
        let weak = Arc::downgrade(&self.inner);
        let interval = self.inner.config.directory_poll;
        tokio::spawn(async move {
            loop {
                let Some(inner) = weak.upgrade() else { return };
                // A cold scan can cost half a second; it does not belong
                // on a runtime worker. Sleeping *after* it also means a
                // slow scan delays the next one instead of stacking.
                if tokio::task::spawn_blocking(move || inner.refresh())
                    .await
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(interval).await;
            }
        });
    }

    /// Root sessions (subagent logs excluded, as in `SessionStore::list`),
    /// most recently modified first.
    pub(crate) fn sessions(&self) -> Vec<SessionEntry> {
        self.select(|head| head.meta.parent_id.is_none())
    }

    /// The subagent sessions one session spawned, newest first.
    pub(crate) fn children(&self, parent_id: &str) -> Vec<SessionEntry> {
        self.select(|head| head.meta.parent_id.as_deref() == Some(parent_id))
    }

    /// One session's head. Falls back to a direct read for a session the
    /// poller has not seen yet — a session created a moment ago is the
    /// most likely one to be asked for. A cached miss is an answer, not
    /// a reason to read again.
    pub(crate) fn head(&self, id: &str) -> Option<SessionEntry> {
        if let Some(cached) = lock(&self.inner.cache).get(id) {
            return cached.head.as_ref().map(entry_of);
        }
        self.inner.read_head(id).ok().as_ref().map(entry_of)
    }

    fn select(&self, keep: impl Fn(&SessionHead) -> bool) -> Vec<SessionEntry> {
        let cache = lock(&self.inner.cache);
        let mut entries: Vec<SessionEntry> = cache
            .values()
            .filter_map(|cached| cached.head.as_ref())
            .filter(|head| keep(head))
            .map(entry_of)
            .collect();
        drop(cache);
        entries.sort_by(|left, right| {
            right
                .head
                .modified
                .cmp(&left.head.modified)
                .then_with(|| left.head.id.cmp(&right.head.id))
        });
        entries
    }

    /// Follow a session: a snapshot to start from and the stream that
    /// continues it. Starts the tailer if this is the first subscriber.
    pub(crate) fn subscribe(&self, id: &str) -> std::io::Result<Subscription> {
        if let Some(tailer) = lock(&self.inner.tailers).get(id) {
            // Still under the map lock: a handoff and `retire_idle` must
            // not interleave, or this subscriber would hold a receiver
            // no task feeds.
            return Ok(tailer.handoff());
        }
        let tail = SessionTail::open(&self.inner.store, id)?;
        let (sender, _) = broadcast::channel(self.inner.config.capacity);
        let tailer = Arc::new(Tailer {
            sender,
            state: Mutex::new(TailState { tail, ended: None }),
        });
        // Prime before anyone can subscribe: the log so far belongs in
        // the snapshot, not in a burst down a 256-slot channel that a
        // 906-event session would overrun on its own first poll. This
        // reads the whole file, so it happens off the map lock — nobody
        // else can see this tailer yet.
        let alive = tailer.step();

        let mut tailers = lock(&self.inner.tailers);
        // Someone else may have finished the same work while this one
        // was reading. Theirs is in the map, so theirs wins.
        if let Some(existing) = tailers.get(id) {
            return Ok(existing.handoff());
        }
        let subscription = tailer.handoff();
        if alive {
            tailers.insert(id.to_string(), tailer.clone());
            drop(tailers);
            self.inner.spawn_tailer(id.to_string(), tailer);
        }
        Ok(subscription)
    }

    /// How many session heads have been parsed since startup. The head
    /// cache's whole justification is that this number stays flat while
    /// nothing changes.
    pub(crate) fn head_reads(&self) -> u64 {
        self.inner.head_reads.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn tailer_count(&self) -> usize {
        lock(&self.inner.tailers).len()
    }
}

impl Inner {
    fn refresh(&self) {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            // The directory is gone: so is everything the cache claims.
            // Any other failure (a descriptor limit, a permission blip)
            // is transient, and stale rows beat an empty listing.
            Err(error) => {
                if error.kind() == std::io::ErrorKind::NotFound {
                    lock(&self.cache).clear();
                }
                return;
            }
        };
        let seen: HashMap<String, Stat> = entries
            .flatten()
            .filter_map(|entry| {
                let id = session_id_of(&entry.file_name())?;
                Some((id, stat_of(&entry.metadata().ok()?)))
            })
            .collect();

        // Diff under the lock, read heads without it: a cold pass parses
        // every head in the store (P8: up to 577 ms) and the listing
        // route must not wait behind that.
        let stale: Vec<(String, Stat)> = {
            let mut cache = lock(&self.cache);
            cache.retain(|id, _| seen.contains_key(id));
            seen.iter()
                .filter(|(id, stat)| cache.get(*id).is_none_or(|cached| cached.stat != **stat))
                .map(|(id, stat)| (id.clone(), *stat))
                .collect()
        };
        for (id, stat) in stale {
            // A file that changed again while its head was being read
            // keeps the stat it was read at, so the next pass sees a
            // mismatch and reads it again.
            match self.read_head(&id) {
                Ok(head) => {
                    lock(&self.cache).insert(
                        id,
                        Cached {
                            stat,
                            head: Some(head),
                        },
                    );
                }
                // Not a session this build can summarize (a foreign
                // file, a torn first line, a log from a newer ilar).
                // Remember the miss so it is not retried every second.
                Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                    lock(&self.cache).insert(id, Cached { stat, head: None });
                }
                // A transient failure — a descriptor limit, a racing
                // rename — must not evict a session from the listing
                // until the process restarts. Leave it stale.
                Err(_) => {}
            }
        }
    }

    fn read_head(&self, id: &str) -> std::io::Result<SessionHead> {
        self.head_reads.fetch_add(1, Ordering::Relaxed);
        self.store.head(id)
    }

    fn spawn_tailer(self: &Arc<Self>, id: String, tailer: Arc<Tailer>) {
        let weak = Arc::downgrade(self);
        let poll = self.config.tail_poll;
        let linger = self.config.idle_linger;
        tokio::spawn(async move {
            let mut idle_since: Option<Instant> = None;
            loop {
                tokio::time::sleep(poll).await;
                let Some(inner) = weak.upgrade() else { return };
                if tailer.subscribers() == 0 {
                    let since = *idle_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= linger {
                        if inner.retire_idle(&id, &tailer) {
                            return;
                        }
                        // Refused: a subscriber arrived between the two
                        // checks, so the linger starts over rather than
                        // firing again on the next tick.
                        idle_since = None;
                    }
                } else {
                    idle_since = None;
                }
                // Usually a stat and a few hundred bytes, but a resync
                // rereads the whole file (`SessionTail::poll` rebuilds
                // from byte 0), which is not a runtime worker's job.
                let stepping = tailer.clone();
                match tokio::task::spawn_blocking(move || stepping.step()).await {
                    Ok(true) => {}
                    Ok(false) => {
                        inner.retire(&id, &tailer);
                        return;
                    }
                    Err(_) => return,
                }
            }
        });
    }

    /// Retire an idle tailer, unless someone subscribed in the meantime.
    /// The check and the removal share the map lock that `subscribe`
    /// takes, which is what makes "no subscribers" still true when the
    /// entry disappears.
    fn retire_idle(&self, id: &str, tailer: &Arc<Tailer>) -> bool {
        let mut tailers = lock(&self.tailers);
        if tailer.subscribers() > 0 {
            return false;
        }
        Self::remove_if_same(&mut tailers, id, tailer);
        true
    }

    fn retire(&self, id: &str, tailer: &Arc<Tailer>) {
        let mut tailers = lock(&self.tailers);
        Self::remove_if_same(&mut tailers, id, tailer);
    }

    fn remove_if_same(tailers: &mut HashMap<String, Arc<Tailer>>, id: &str, tailer: &Arc<Tailer>) {
        if tailers
            .get(id)
            .is_some_and(|current| Arc::ptr_eq(current, tailer))
        {
            tailers.remove(id);
        }
    }
}

/// The session id a directory entry names, or `None` for anything that
/// is not a session log — sidecars, locks, and foreign files included.
fn session_id_of(name: &std::ffi::OsStr) -> Option<String> {
    let id = name.to_str()?.strip_suffix(".jsonl")?;
    SessionId::parse(id).ok().map(|id| id.as_str().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ilar::session::{ContentBlock, SessionEvent, SessionMeta, Usage, new_id};

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        store: SessionStore,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions");
        std::fs::create_dir_all(&root).unwrap();
        Fixture {
            _dir: dir,
            store: SessionStore::new(root.clone()),
            root,
        }
    }

    /// Short intervals so a test observes a loop instead of waiting for
    /// one; the shipped values are the `Default`.
    fn fast() -> WatchConfig {
        WatchConfig {
            directory_poll: Duration::from_millis(10),
            tail_poll: Duration::from_millis(5),
            idle_linger: Duration::from_millis(40),
            capacity: 8,
        }
    }

    /// Every fixture session is written by the real writer, so the tests
    /// exercise the true append path.
    fn start(store: &SessionStore) -> (String, ilar::session::Session) {
        let id = new_id();
        let session = store
            .create(SessionMeta {
                session_id: id.clone(),
                parent_id: None,
                agent: "build".into(),
                model: "test/model".into(),
                workspace: None,
                cwd: Some(PathBuf::from("/tmp/project")),
            })
            .unwrap();
        (id, session)
    }

    fn child(store: &SessionStore, parent_id: &str) -> (String, ilar::session::Session) {
        let id = new_id();
        let session = store
            .create(SessionMeta {
                session_id: id.clone(),
                parent_id: Some(parent_id.to_string()),
                agent: "explore".into(),
                model: "test/model".into(),
                workspace: None,
                cwd: None,
            })
            .unwrap();
        (id, session)
    }

    fn user(text: &str) -> SessionEvent {
        SessionEvent::UserMessage {
            id: new_id(),
            text: text.into(),
            images: Vec::new(),
            ts: chrono::Utc::now(),
        }
    }

    fn assistant(text: &str) -> SessionEvent {
        SessionEvent::AssistantMessage {
            id: new_id(),
            model: "test/model".into(),
            content: vec![ContentBlock::Text { text: text.into() }],
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
            ts: chrono::Utc::now(),
        }
    }

    fn text_of(message: &TailMessage) -> String {
        match message {
            TailMessage::Update(TailUpdate::Appended {
                event: SessionEvent::UserMessage { text, .. },
                ..
            }) => text.clone(),
            other => panic!("expected a user message, got {other:?}"),
        }
    }

    fn line_of(message: &TailMessage) -> usize {
        match message {
            TailMessage::Update(
                TailUpdate::Appended { line, .. } | TailUpdate::Rewound { line, .. },
            ) => *line,
            other => panic!("expected a line-bearing update, got {other:?}"),
        }
    }

    /// Wait for a condition the background loops are expected to reach,
    /// rather than sleeping for a fixed guess.
    async fn until(mut ready: impl FnMut() -> bool) {
        for _ in 0..400 {
            if ready() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("condition never held");
    }

    async fn next(receiver: &mut broadcast::Receiver<TailMessage>) -> TailMessage {
        tokio::time::timeout(Duration::from_secs(5), next_message(receiver))
            .await
            .expect("a message within five seconds")
            .expect("the tailer is still alive")
    }

    #[tokio::test]
    async fn appends_reach_a_subscriber_in_order() {
        let fixture = fixture();
        let (id, mut session) = start(&fixture.store);
        let watcher = Watcher::new(fixture.root.clone(), fast());

        let mut subscription = watcher.subscribe(&id).unwrap();
        assert_eq!(subscription.line, 1, "the meta line is already snapshot");
        assert_eq!(subscription.events.len(), 1);

        session.append(user("one")).unwrap();
        session.append(assistant("did one")).unwrap();
        session.append(user("two")).unwrap();

        let mut seen = Vec::new();
        for _ in 0..3 {
            let message = next(&mut subscription.receiver).await;
            seen.push(line_of(&message));
        }
        assert_eq!(seen, [2, 3, 4], "in file order, one line each");
    }

    /// The handoff decision, tested: a late subscriber gets the folded
    /// view as it stands plus everything after it — no replay of what it
    /// already has, no gap where the two meet.
    #[tokio::test]
    async fn a_second_subscriber_starts_from_a_snapshot_not_a_replay() {
        let fixture = fixture();
        let (id, mut session) = start(&fixture.store);
        let watcher = Watcher::new(fixture.root.clone(), fast());

        let mut first = watcher.subscribe(&id).unwrap();
        session.append(user("one")).unwrap();
        assert_eq!(text_of(&next(&mut first.receiver).await), "one");

        let mut second = watcher.subscribe(&id).unwrap();
        assert_eq!(second.line, 2, "starts where the tail actually stands");
        assert_eq!(
            second.events,
            fixture.store.load(&id).unwrap().events(),
            "the snapshot is the store's own folded view"
        );

        session.append(user("two")).unwrap();
        let message = next(&mut second.receiver).await;
        assert_eq!(text_of(&message), "two", "no replay of line 2");
        assert_eq!(line_of(&message), 3);
        assert_eq!(text_of(&next(&mut first.receiver).await), "two");
    }

    /// A rewind reaches subscribers as the marker it is, so a client can
    /// truncate its own folded copy.
    #[tokio::test]
    async fn a_rewind_reaches_the_subscriber_as_a_fold() {
        let fixture = fixture();
        let (id, mut session) = start(&fixture.store);
        session.append(user("one")).unwrap();
        session.append(assistant("did one")).unwrap();
        // The rewind path takes the writer lease for itself.
        drop(session);
        let watcher = Watcher::new(fixture.root.clone(), fast());
        let mut subscription = watcher.subscribe(&id).unwrap();
        assert_eq!(subscription.events.len(), 3);

        let SessionEvent::UserMessage { id: target, .. } = subscription.events[1].clone() else {
            panic!("expected the turn to rewind at index 1");
        };
        ilar::rewind::rewind_session(&fixture.store, &id, 1, &target, &fixture.root)
            .await
            .unwrap();
        let message = next(&mut subscription.receiver).await;
        assert!(
            matches!(
                message,
                TailMessage::Update(TailUpdate::Rewound { line: 4, to: 1, .. })
            ),
            "got {message:?}"
        );
    }

    /// Overrunning the channel must be loud: the subscriber is told its
    /// view is stale, never quietly handed the next line as if nothing
    /// had been dropped.
    #[tokio::test]
    async fn a_lagging_subscriber_is_told_to_resync() {
        let fixture = fixture();
        let (id, mut session) = start(&fixture.store);
        let config = fast();
        let watcher = Watcher::new(fixture.root.clone(), config);
        let mut subscription = watcher.subscribe(&id).unwrap();

        for index in 0..config.capacity * 3 {
            session.append(user(&format!("line {index}"))).unwrap();
        }
        until(|| subscription.receiver.len() >= config.capacity).await;

        assert_eq!(
            next(&mut subscription.receiver).await,
            TailMessage::Update(TailUpdate::Resync),
            "a dropped line is a resync, not a silent gap"
        );
    }

    #[tokio::test]
    async fn a_deleted_session_closes_the_stream() {
        let fixture = fixture();
        let (id, session) = start(&fixture.store);
        drop(session);
        let watcher = Watcher::new(fixture.root.clone(), fast());
        let mut subscription = watcher.subscribe(&id).unwrap();

        fixture.store.delete(&id).unwrap();
        assert_eq!(
            next(&mut subscription.receiver).await,
            TailMessage::Update(TailUpdate::Deleted)
        );
        assert!(
            tokio::time::timeout(
                Duration::from_secs(5),
                next_message(&mut subscription.receiver)
            )
            .await
            .expect("the channel closes")
            .is_none(),
            "deletion is terminal"
        );
        until(|| watcher.tailer_count() == 0).await;

        watcher.refresh();
        assert!(watcher.sessions().is_empty(), "and it leaves the listing");
        assert_eq!(
            watcher.subscribe(&id).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
    }

    /// The forward-compat rule: one session's unreadable line ends that
    /// session's tail with the store's own words, and touches nothing
    /// else.
    #[tokio::test]
    async fn a_line_from_a_newer_ilar_ends_only_that_tail() {
        use std::io::Write;

        let fixture = fixture();
        let (future_id, future_session) = start(&fixture.store);
        let (plain_id, mut plain_session) = start(&fixture.store);
        drop(future_session);
        let watcher = Watcher::new(fixture.root.clone(), fast());

        let mut doomed = watcher.subscribe(&future_id).unwrap();
        let mut healthy = watcher.subscribe(&plain_id).unwrap();

        let path = fixture.store.session_path(&future_id).unwrap();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"{\"type\":\"telepathy\",\"id\":\"x\"}\n")
            .unwrap();
        file.sync_data().unwrap();

        let message = next(&mut doomed.receiver).await;
        let TailMessage::Failed(text) = &message else {
            panic!("expected a terminal failure, got {message:?}");
        };
        assert!(
            text.contains("written by a newer ilar?") && text.contains("telepathy"),
            "the store's own diagnostic travels to the client: {text}"
        );
        until(|| watcher.tailer_count() == 1).await;

        plain_session.append(user("still here")).unwrap();
        assert_eq!(
            text_of(&next(&mut healthy.receiver).await),
            "still here",
            "the other session never noticed"
        );

        // A subscriber arriving after the end still learns why — this
        // one re-opens the file and hits the same wall on its priming
        // poll, which is the same answer by a longer road.
        let late = watcher.subscribe(&future_id).unwrap();
        assert!(matches!(late.ended, Some(TailEnd::Failed(_))));
        assert_eq!(watcher.tailer_count(), 1, "and it is not registered");
    }

    /// The store repaired or replaced the file under a live tailer:
    /// nothing about the old view survives, and the subscriber is told
    /// so rather than handed lines that no longer follow from what it
    /// has.
    #[tokio::test]
    async fn a_rebuilt_file_reaches_the_subscriber_as_a_resync() {
        let fixture = fixture();
        let (id, mut session) = start(&fixture.store);
        session.append(user("first")).unwrap();
        session.append(assistant("did first")).unwrap();
        drop(session);
        let watcher = Watcher::new(fixture.root.clone(), fast());
        let mut subscription = watcher.subscribe(&id).unwrap();
        assert_eq!(subscription.line, 3);

        // Committed bytes vanish — P5 says the store never does this, so
        // the reader's whole view is suspect.
        let path = fixture.store.session_path(&id).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let keep = bytes.iter().filter(|byte| **byte == b'\n').count();
        assert_eq!(keep, 3);
        let cut = bytes.iter().position(|byte| *byte == b'\n').unwrap() + 1;
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(cut as u64)
            .unwrap();

        assert_eq!(
            next(&mut subscription.receiver).await,
            TailMessage::Update(TailUpdate::Resync)
        );
        // And the rebuilt tail is coherent for whoever asks next.
        let rebuilt = watcher.subscribe(&id).unwrap();
        assert_eq!(rebuilt.line, 1);
        assert_eq!(rebuilt.events, fixture.store.audit_events(&id).unwrap());
    }

    #[tokio::test]
    async fn a_tailer_retires_after_its_last_subscriber_leaves() {
        let fixture = fixture();
        let (id, session) = start(&fixture.store);
        drop(session);
        let watcher = Watcher::new(fixture.root.clone(), fast());

        let subscription = watcher.subscribe(&id).unwrap();
        assert_eq!(watcher.tailer_count(), 1);
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(watcher.tailer_count(), 1, "a subscriber keeps it warm");

        drop(subscription);
        until(|| watcher.tailer_count() == 0).await;

        // And a later reader starts a fresh one from the current file.
        let subscription = watcher.subscribe(&id).unwrap();
        assert_eq!(subscription.line, 1);
        assert_eq!(watcher.tailer_count(), 1);
    }

    /// The linger's point: a reload lands on the warm tail, not on a
    /// fresh one. The proof is that the tailer is never rebuilt — a
    /// rebuild would be invisible from the outside except for the extra
    /// full re-read it costs.
    #[tokio::test]
    async fn a_reload_inside_the_linger_reuses_the_warm_tailer() {
        let fixture = fixture();
        let (id, mut session) = start(&fixture.store);
        let watcher = Watcher::new(fixture.root.clone(), fast());

        let first = watcher.subscribe(&id).unwrap();
        drop(first);
        // Inside the linger window: the tailer is idle but alive.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(watcher.tailer_count(), 1);

        let mut second = watcher.subscribe(&id).unwrap();
        session.append(user("after the reload")).unwrap();
        assert_eq!(
            text_of(&next(&mut second.receiver).await),
            "after the reload"
        );

        // The idle clock restarted with the new subscriber rather than
        // firing on the next tick.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(watcher.tailer_count(), 1);
        drop(second);
        until(|| watcher.tailer_count() == 0).await;
    }

    #[tokio::test]
    async fn a_new_session_shows_up_in_the_listing_within_a_tick() {
        let fixture = fixture();
        let watcher = Watcher::new(fixture.root.clone(), fast());
        watcher.spawn_poller();

        let (id, mut session) = start(&fixture.store);
        session.append(user("what does this do?")).unwrap();
        until(|| !watcher.sessions().is_empty()).await;

        let sessions = watcher.sessions();
        assert_eq!(sessions.len(), 1);
        let entry = &sessions[0];
        assert_eq!(entry.head.id, id);
        assert_eq!(entry.head.title.as_deref(), Some("what does this do?"));
        assert_eq!(entry.head.meta.agent, "build");
        assert_eq!(
            entry.head.meta.cwd.as_deref(),
            Some(std::path::Path::new("/tmp/project"))
        );
        assert!(entry.live, "a session written a moment ago is live");
    }

    /// P8's reason for the cache: a head parse is expensive, so an
    /// unchanged session must never be parsed twice.
    #[tokio::test]
    async fn the_head_cache_re_reads_only_changed_sessions() {
        let fixture = fixture();
        let (first_id, mut first) = start(&fixture.store);
        let (_second_id, second) = start(&fixture.store);
        drop(second);
        let watcher = Watcher::new(fixture.root.clone(), fast());

        watcher.refresh();
        assert_eq!(watcher.head_reads(), 2, "one head per session, once");
        watcher.refresh();
        watcher.refresh();
        assert_eq!(watcher.head_reads(), 2, "nothing changed, nothing re-read");

        first.append(user("a new turn")).unwrap();
        watcher.refresh();
        assert_eq!(watcher.head_reads(), 3, "only the session that moved");
        assert_eq!(
            watcher
                .sessions()
                .iter()
                .find(|entry| entry.head.id == first_id)
                .and_then(|entry| entry.head.title.clone())
                .as_deref(),
            Some("a new turn"),
            "and the re-read landed in the cache"
        );

        drop(first);
        fixture.store.delete(&first_id).unwrap();
        watcher.refresh();
        assert_eq!(watcher.sessions().len(), 1);
        assert_eq!(watcher.head_reads(), 3, "a deletion costs no head read");
    }

    /// A file this build cannot summarize is remembered as a miss, not
    /// retried every second — and never shows up as half a session.
    #[tokio::test]
    async fn an_unreadable_session_file_is_head_read_once() {
        let fixture = fixture();
        let (id, session) = start(&fixture.store);
        drop(session);
        let unreadable = fixture.root.join(format!("{}.jsonl", new_id()));
        std::fs::write(&unreadable, b"not a session at all\n").unwrap();
        let watcher = Watcher::new(fixture.root.clone(), fast());

        watcher.refresh();
        assert_eq!(watcher.head_reads(), 2);
        watcher.refresh();
        assert_eq!(watcher.head_reads(), 2, "the miss is cached too");
        assert_eq!(
            watcher
                .sessions()
                .iter()
                .map(|entry| entry.head.id.clone())
                .collect::<Vec<_>>(),
            vec![id]
        );
    }

    /// The listing hides subagent logs the way `SessionStore::list`
    /// does; the children route is the other half of the same cache.
    #[tokio::test]
    async fn children_are_kept_out_of_the_listing_and_found_by_parent() {
        let fixture = fixture();
        let (parent_id, parent) = start(&fixture.store);
        let (child_id, mut child_session) = child(&fixture.store, &parent_id);
        child_session.append(user("review this")).unwrap();
        drop(parent);
        drop(child_session);
        let watcher = Watcher::new(fixture.root.clone(), fast());
        watcher.refresh();

        let sessions = watcher.sessions();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].head.id, parent_id);

        let children = watcher.children(&parent_id);
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].head.id, child_id);
        assert_eq!(children[0].head.meta.agent, "explore");
        assert!(watcher.children(&child_id).is_empty());
        assert_eq!(
            watcher.head(&child_id).map(|entry| entry.head.id),
            Some(child_id)
        );
    }

    /// A session the poller has not reached yet is still answerable.
    #[tokio::test]
    async fn a_head_is_readable_before_the_first_scan() {
        let fixture = fixture();
        let (id, session) = start(&fixture.store);
        drop(session);
        let watcher = Watcher::new(fixture.root.clone(), fast());

        assert_eq!(watcher.head(&id).map(|entry| entry.head.id), Some(id));
        assert!(watcher.head(&new_id()).is_none());
    }

    #[test]
    fn the_poll_interval_flag_outranks_the_environment() {
        assert_eq!(WatchConfig::resolve(None, None), WatchConfig::default());
        assert_eq!(
            WatchConfig::resolve(None, Some("40")),
            WatchConfig::with_poll_ms(40)
        );
        assert_eq!(
            WatchConfig::resolve(Some(10), Some("40")),
            WatchConfig::with_poll_ms(10)
        );
        // Nonsense is ignored, not obeyed into a busy loop.
        assert_eq!(
            WatchConfig::resolve(None, Some("soon")),
            WatchConfig::default()
        );
        assert_eq!(
            WatchConfig::resolve(None, Some("0")),
            WatchConfig::default()
        );
        assert_eq!(
            WatchConfig::with_poll_ms(50).directory_poll,
            Duration::from_millis(200),
            "the shipped 1:4 ratio survives tuning"
        );
    }
}
