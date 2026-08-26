//! The live turn's scratch sidecar: `sessions/<id>.live`.
//!
//! The store is the wire (see meta/issues/ilar-serve-reads-the-store.md),
//! but a committed line only appears when a step ends — so a watcher can
//! see steps and never tokens. This module is the other half: while a
//! turn runs, the loop writes batched deltas to an ephemeral sidecar next
//! to the session log, and anything tailing the store tails that too.
//!
//! The rules that make it safe to write from inside a turn:
//!
//! - **It is not the audit log.** Nothing here is ever replayed, listed,
//!   or read back by the store; the `.jsonl` remains the only durable
//!   record. Deliberately tiny variants — anything richer waits for the
//!   committed event.
//! - **Streaming is a luxury, the turn is not.** Every IO failure is
//!   swallowed, and the first one retires the scratch for the rest of the
//!   turn ([`LiveScratch::disable`]). Nothing in here returns an error.
//! - **Never fsynced**, buffered, and flushed on a deadline checked at
//!   write time (~150 ms) or at 4 KiB — no background timer, so a turn
//!   that stops streaming stops writing.
//! - **Truncate means "reset"**: a step commit empties the file, because
//!   everything it held has just landed on the main stream. Deletion
//!   means the turn ended, and a drop guard makes that true for every
//!   outcome — completion, abort, or a panic unwinding through the loop.
//!
//! [`LiveDelta::TurnStarted`] names the generation it opens. A reader
//! compares it against the one it last saw, which is what tells a reset
//! from an append even when the new generation has already outgrown the
//! old one between two polls — a length comparison alone would splice
//! one generation's lines onto another's.

use std::io::{Seek, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use super::store::SessionStore;

/// What a scratch file is called, next to the `.jsonl` it belongs to.
pub const LIVE_SUFFIX: &str = ".live";

/// Batched appends land at most this far apart while a turn streams.
/// ~150 ms against a 250 ms poll reads as live across a network.
const FLUSH_INTERVAL: Duration = Duration::from_millis(150);
/// …or sooner, once this much is buffered.
const FLUSH_BYTES: usize = 4 * 1024;

/// How long a scratch may sit before a startup sweep treats it as a
/// crash leftover. Generously long on purpose: a turn stuck in a very
/// slow tool is quiet but alive, and deleting *its* scratch would make a
/// running turn read as idle. A crash leftover only costs a "stalled"
/// row until the next `ilar` starts.
pub const SCRATCH_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

/// One line of a scratch file. Small by design: a reader that wants more
/// than this waits for the committed event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LiveDelta {
    /// First line of every generation, naming it. `turn` is unique to
    /// this scratch and `step` counts the truncations within it; a
    /// reader that sees the pair change knows its offset means nothing
    /// any more.
    ///
    /// Both halves are load-bearing. `step` alone would not do: a turn
    /// that aborts during its first step never commits, so its
    /// generation and the next turn's first one are both step 0, and a
    /// reader would splice one turn's text onto the other's. `turn`
    /// alone would not do either — a step commit has to reset a reader
    /// in the middle of a turn.
    TurnStarted {
        turn: String,
        step: u64,
    },
    TextDelta {
        text: String,
    },
    /// Reasoning text, raw or summarized — the two render the same and
    /// neither is ever replayed to a provider.
    ThinkingDelta {
        text: String,
    },
    ToolStarted {
        id: String,
        name: String,
        /// The same one-line, redacted summary the transcript will carry
        /// ([`crate::agent::summarize_tool_input`]).
        summary: String,
    },
    ToolFinished {
        id: String,
        ok: bool,
    },
}

/// The scratch path beside a session log: `<id>.jsonl` → `<id>.live`.
pub fn live_path(session_path: &Path) -> PathBuf {
    let mut name = session_path
        .file_stem()
        .unwrap_or(session_path.as_os_str())
        .to_os_string();
    name.push(LIVE_SUFFIX);
    session_path.with_file_name(name)
}

/// Everything committed to a scratch file so far. Incomplete trailing
/// bytes and lines this build cannot parse are skipped rather than
/// reported: a reader must never fail on a file that is only a hint.
pub fn parse_scratch(bytes: &[u8]) -> Vec<LiveDelta> {
    bytes
        .split_inclusive(|byte| *byte == b'\n')
        .filter(|line| line.ends_with(b"\n"))
        .filter_map(|line| serde_json::from_slice(line).ok())
        .collect()
}

/// The turn's writer. Created at turn start, truncated at every step
/// commit, deleted on drop.
#[derive(Debug)]
pub struct LiveScratch {
    path: PathBuf,
    /// `None` once retired: the first IO failure disables the scratch
    /// for the rest of the turn, and every method below is a no-op.
    file: Option<std::fs::File>,
    buffer: String,
    due: std::time::Instant,
    /// This scratch's half of the generation, fixed for its lifetime.
    turn: String,
    step: u64,
}

impl LiveScratch {
    /// Open the scratch beside session `id`'s log. A store that cannot
    /// name the path, or a path that cannot be created, yields a
    /// disabled scratch — the caller cannot tell, and must not care.
    pub fn start(store: &SessionStore, id: &str) -> Self {
        match store.session_path(id) {
            Ok(path) => Self::create(live_path(&path)),
            Err(_) => Self::create(PathBuf::new()),
        }
    }

    /// Create (or empty) the scratch at `path`.
    pub fn create(path: PathBuf) -> Self {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .ok();
        let mut scratch = Self {
            path,
            file,
            buffer: String::new(),
            due: std::time::Instant::now() + FLUSH_INTERVAL,
            turn: super::event::new_id(),
            step: 0,
        };
        scratch.open_generation();
        scratch
    }

    fn open_generation(&mut self) {
        self.mark(LiveDelta::TurnStarted {
            turn: self.turn.clone(),
            step: self.step,
        });
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn text(&mut self, text: &str) {
        self.push(LiveDelta::TextDelta { text: text.into() }, false);
    }

    pub fn thinking(&mut self, text: &str) {
        self.push(LiveDelta::ThinkingDelta { text: text.into() }, false);
    }

    /// A marker, flushed at once: it is what a supervisor reads as the
    /// turn's current activity, and it is far too small to batch.
    pub fn tool_started(&mut self, id: &str, name: &str, summary: &str) {
        self.mark(LiveDelta::ToolStarted {
            id: id.into(),
            name: name.into(),
            summary: summary.into(),
        });
    }

    pub fn tool_finished(&mut self, id: &str, ok: bool) {
        self.mark(LiveDelta::ToolFinished { id: id.into(), ok });
    }

    /// The step committed. Everything the scratch held is now on the
    /// main stream, so the file starts over at the next generation.
    pub fn commit(&mut self) {
        self.buffer.clear();
        self.step += 1;
        let reset = self.file.as_mut().map(|file| {
            file.set_len(0)
                .and_then(|()| file.seek(std::io::SeekFrom::Start(0)))
        });
        match reset {
            Some(Err(_)) => self.disable(),
            _ => self.open_generation(),
        }
    }

    fn mark(&mut self, delta: LiveDelta) {
        self.push(delta, true);
    }

    fn push(&mut self, delta: LiveDelta, force: bool) {
        if self.file.is_none() {
            return;
        }
        let Ok(line) = serde_json::to_string(&delta) else {
            return;
        };
        self.buffer.push_str(&line);
        self.buffer.push('\n');
        if force || self.buffer.len() >= FLUSH_BYTES || std::time::Instant::now() >= self.due {
            self.flush();
        }
    }

    fn flush(&mut self) {
        let Some(file) = self.file.as_mut() else {
            self.buffer.clear();
            return;
        };
        if file.write_all(self.buffer.as_bytes()).is_err() {
            self.disable();
            return;
        }
        self.buffer.clear();
        self.due = std::time::Instant::now() + FLUSH_INTERVAL;
    }

    /// Retire the scratch after an IO failure, and take the file with
    /// it: a scratch nobody is writing to any more would otherwise read
    /// as a stalled turn for as long as the turn runs.
    fn disable(&mut self) {
        self.file = None;
        self.buffer.clear();
        self.remove();
    }

    fn remove(&self) {
        if !self.path.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// The turn ended — however it ended. A drop guard rather than a call at
/// the end of the loop, because an abort, an error return and a panic
/// unwinding through `run_turn` all have to clean up too.
impl Drop for LiveScratch {
    fn drop(&mut self) {
        self.remove();
    }
}

/// Delete scratch files left behind by turns that never got to drop
/// theirs. Runs at startup beside the spill sweep, and like it, never
/// fails anything: a state directory that cannot be read simply has
/// nothing to clean.
pub fn sweep_live_scratches(dir: &Path) {
    let Some(cutoff) = SystemTime::now().checked_sub(SCRATCH_RETENTION) else {
        return;
    };
    remove_scratches_before(dir, cutoff);
}

fn remove_scratches_before(dir: &Path, cutoff: SystemTime) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "live") {
            continue;
        }
        let stale = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .is_some_and(|modified| modified < cutoff);
        if stale {
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(path: &Path) -> Vec<LiveDelta> {
        parse_scratch(&std::fs::read(path).unwrap_or_default())
    }

    /// The generation a scratch file currently holds, and everything
    /// written into it since.
    fn generation(path: &Path) -> ((String, u64), Vec<LiveDelta>) {
        let deltas = read(path);
        let [LiveDelta::TurnStarted { turn, step }, rest @ ..] = &deltas[..] else {
            panic!("every generation opens with turn_started, got {deltas:?}");
        };
        ((turn.clone(), *step), rest.to_vec())
    }

    #[test]
    fn a_scratch_path_sits_beside_the_session_log() {
        assert_eq!(
            live_path(Path::new("/state/sessions/abc-123.jsonl")),
            Path::new("/state/sessions/abc-123.live")
        );
    }

    /// The lifecycle in one test: written, truncated at a commit, gone
    /// at drop — and every line parses back into what was written.
    #[test]
    fn a_turn_writes_truncates_and_deletes_its_scratch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("turn.live");
        let mut scratch = LiveScratch::create(path.clone());

        scratch.tool_started("call-1", "bash", "cargo test");
        scratch.text("hel");
        scratch.text("lo");
        scratch.tool_finished("call-1", true);
        let (first, written) = generation(&path);
        assert_eq!(first.1, 0);
        assert_eq!(
            written,
            vec![
                LiveDelta::ToolStarted {
                    id: "call-1".into(),
                    name: "bash".into(),
                    summary: "cargo test".into(),
                },
                LiveDelta::TextDelta { text: "hel".into() },
                LiveDelta::TextDelta { text: "lo".into() },
                LiveDelta::ToolFinished {
                    id: "call-1".into(),
                    ok: true,
                },
            ],
            "a marker flushes everything buffered behind it"
        );

        scratch.commit();
        let (second, written) = generation(&path);
        assert_eq!(second, (first.0.clone(), 1), "same turn, the next step");
        assert!(written.is_empty(), "the commit emptied the file");
        scratch.thinking("second step");
        scratch.tool_started("call-2", "read", "src/lib.rs");
        assert_eq!(
            generation(&path).1.len(),
            2,
            "and the file grows again from there"
        );

        drop(scratch);
        assert!(!path.exists(), "the turn ended, so the scratch is gone");
    }

    /// A generation is never mistaken for an append, even when the new
    /// one is already longer than the old one was — including across
    /// turns, where the step counter alone starts over at zero.
    #[test]
    fn every_generation_names_itself_in_its_first_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("turn.live");
        let mut scratch = LiveScratch::create(path.clone());
        scratch.text("x");
        scratch.commit();
        scratch.tool_started("call-1", "bash", "cargo test");
        scratch.commit();
        let (first, _) = generation(&path);
        assert_eq!(first.1, 2);

        // A turn that aborts before committing anything, and the next
        // turn on the same session: both are step 0, and they must not
        // look like one continuing file.
        drop(scratch);
        let aborted = LiveScratch::create(path.clone());
        let (aborted_generation, _) = generation(&path);
        assert_eq!(aborted_generation.1, 0);
        drop(aborted);

        let _next = LiveScratch::create(path.clone());
        let (next, _) = generation(&path);
        assert_eq!(next.1, 0, "a fresh turn starts its own count");
        assert_ne!(
            next.0, aborted_generation.0,
            "but never reuses the generation the abandoned turn had"
        );
    }

    /// The whole failure policy: a scratch that cannot be opened is
    /// silently dead, and every call on it still returns.
    #[test]
    fn a_scratch_that_cannot_be_written_retires_itself() {
        let dir = tempfile::tempdir().unwrap();
        // A directory where the file belongs: creation fails, exactly as
        // it would on a full or read-only disk.
        let path = dir.path().join("blocked.live");
        std::fs::create_dir(&path).unwrap();

        let mut scratch = LiveScratch::create(path.clone());
        scratch.text("hello");
        scratch.tool_started("call-1", "bash", "cargo test");
        scratch.commit();
        scratch.tool_finished("call-1", false);
        drop(scratch);

        assert!(path.is_dir(), "nothing was written, nothing was removed");
        // And a store that cannot even name a path is the same story.
        let store = SessionStore::new(dir.path().to_path_buf());
        let mut nameless = LiveScratch::start(&store, "not-a-uuid");
        nameless.text("hello");
    }

    /// A crash leftover is swept; a scratch from a turn that may still
    /// be running, and anything that is not a scratch at all, is not.
    #[test]
    fn the_startup_sweep_removes_only_stale_scratches() {
        let dir = tempfile::tempdir().unwrap();
        let aged = |name: &str, age: Duration| {
            let path = dir.path().join(name);
            std::fs::write(
                &path,
                b"{\"type\":\"turn_started\",\"turn\":\"t-1\",\"step\":0}\n",
            )
            .unwrap();
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(SystemTime::now() - age))
                .unwrap();
            path
        };
        let day = Duration::from_secs(24 * 60 * 60);
        let stale = aged("old.live", SCRATCH_RETENTION + day);
        let fresh = aged("new.live", Duration::from_secs(90));
        let log = aged("old.jsonl", SCRATCH_RETENTION + day);

        remove_scratches_before(dir.path(), SystemTime::now() - SCRATCH_RETENTION);

        assert!(!stale.exists(), "a crash leftover survived the sweep");
        assert!(fresh.exists(), "a live turn's scratch was swept");
        assert!(log.exists(), "the sweep touched the audit log");

        // And a state directory that never ran a turn has nothing to do.
        sweep_live_scratches(&dir.path().join("never-used"));
    }
}
