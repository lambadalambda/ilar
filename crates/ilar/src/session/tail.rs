//! Incremental, read-only tail of one session's JSONL log.
//!
//! The store's own reader ([`SessionStore::load`]) is a snapshot reader:
//! it validates the whole log against a checkpoint whose stamp discipline
//! belongs to the writer, so it fails most calls while a turn is running.
//! A watcher needs the opposite — never fail, never touch the writer's
//! cache, and see each committed line exactly once.
//!
//! The discipline is a stat snapshot per poll: read exactly the bytes the
//! stat promised, cut at the last newline, parse only complete lines, and
//! advance by committed bytes only. No fd is held between polls (the
//! writer holds an append fd for the session's life, which is also why
//! FSEvents cannot see the appends — see docs/sessions.md). Only the
//! `.jsonl` is ever opened: the `.replay.*` sidecars are writer cache.

use std::io::{Read, Seek};
use std::path::{Path, PathBuf};

use super::event::SessionEvent;
use super::replay_index::committed_line_count;
use super::store::{SessionStore, committed_len, fold_rewinds, parse_event_bytes};

/// One committed line, as the reader saw it.
// The event-carrying variant is the overwhelmingly common one, and
// boxing it would cost an allocation per line to save bytes on the two
// rare variants.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum TailUpdate {
    /// A new event, at physical line `line` (1-based).
    Appended { line: usize, event: SessionEvent },
    /// A rewind marker at physical line `line`, already folded into
    /// [`SessionTail::events`]: the canonical stream was truncated to
    /// `to`. The marker itself is carried too — the fold removes it from
    /// the view, but it is a canonical line a client still renders (and
    /// it names the trees a rewind restored and saved).
    Rewound {
        line: usize,
        to: usize,
        event: SessionEvent,
    },
    /// The file no longer agrees with what was consumed — it shrank
    /// (a writer repaired a torn tail) or was replaced. State has been
    /// rebuilt from byte 0; the whole view is new.
    Resync,
    /// The session file is gone. Terminal: later polls yield nothing.
    Deleted,
}

/// A session log being followed forward.
#[derive(Debug)]
pub struct SessionTail {
    id: String,
    path: PathBuf,
    /// Bytes consumed. Always a committed prefix — a partial trailing
    /// line is left for a later poll.
    offset: u64,
    /// Physical lines consumed, including the ones rewinds folded away.
    line: usize,
    /// The folded canonical view: file order, rewind markers applied.
    events: Vec<SessionEvent>,
    identity: FileIdentity,
    deleted: bool,
}

impl SessionTail {
    /// Follow a session from its first line; the first [`Self::poll`]
    /// delivers the whole log.
    pub fn open(store: &SessionStore, id: &str) -> std::io::Result<Self> {
        Self::open_at(store, id, 0)
    }

    /// Follow a session from physical line `line`: the first `line`
    /// lines are replayed into the folded view without being reported,
    /// and polling resumes after them. This is what a reconnecting
    /// client's last-seen line number resumes on.
    pub fn open_at(store: &SessionStore, id: &str, line: usize) -> std::io::Result<Self> {
        let path = store.session_path(id)?;
        let metadata = std::fs::metadata(&path)?;
        let mut tail = Self {
            id: id.to_string(),
            path,
            offset: 0,
            line: 0,
            events: Vec::new(),
            identity: file_identity(&metadata),
            deleted: false,
        };
        if line > 0 {
            tail.replay(Some(line))?;
        }
        Ok(tail)
    }

    /// The folded canonical event stream: every committed line so far,
    /// with rewind markers applied. Compaction is *not* applied — the
    /// cut is a rendering decision, and the audit history stays visible.
    pub fn events(&self) -> &[SessionEvent] {
        &self.events
    }

    /// Physical lines consumed. The next appended line is `line() + 1`.
    pub fn line(&self) -> usize {
        self.line
    }

    /// Everything committed since the last poll.
    pub fn poll(&mut self) -> std::io::Result<Vec<TailUpdate>> {
        if self.deleted {
            return Ok(Vec::new());
        }
        let metadata = match std::fs::metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.deleted = true;
                return Ok(vec![TailUpdate::Deleted]);
            }
            Err(error) => return Err(error),
        };
        let identity = file_identity(&metadata);
        let len = metadata.len();
        // A shrink means bytes this reader already committed are gone.
        // The writer only ever truncates its own torn tail, which is by
        // definition uncommitted and so was never consumed here — but if
        // it happens anyway, the only honest answer is a fresh read.
        if identity != self.identity || len < self.offset {
            // The identity is adopted only once the rebuild it describes
            // succeeded: a failed replay (an unparsable line, a vanished
            // file) must leave the trigger armed, or the next poll would
            // splice a new file's bytes onto the old file's state.
            self.replay(None)?;
            self.identity = identity;
            return Ok(vec![TailUpdate::Resync]);
        }
        if len == self.offset {
            return Ok(Vec::new());
        }
        // The stat's length governs this read: bytes appended after it
        // belong to the next poll, so the tail never races the writer.
        let chunk = read_at(&self.path, self.offset, len - self.offset)?;
        let committed = &chunk[..committed_len(&chunk)];
        if committed.is_empty() {
            return Ok(Vec::new());
        }
        self.consume(committed)
    }

    /// Fold one committed slab into the view. Nothing is applied until
    /// every line in it parses: a corrupt committed line is an error the
    /// next poll sees again, never a half-consumed slab.
    fn consume(&mut self, committed: &[u8]) -> std::io::Result<Vec<TailUpdate>> {
        let mut parsed: Vec<(usize, SessionEvent)> = Vec::new();
        let mut lines = 0;
        for (index, raw) in committed.split_inclusive(|byte| *byte == b'\n').enumerate() {
            let line = self.line + index + 1;
            lines = index + 1;
            // One line at a time: a blank line yields no event but still
            // costs a physical line number, and diagnostics have to name
            // the line the file actually holds.
            for event in parse_event_bytes(raw, &self.id, line - 1)? {
                parsed.push((line, event));
            }
        }
        let mut updates = Vec::with_capacity(parsed.len());
        for (line, event) in parsed {
            match event {
                // The incremental form of `fold_rewinds`: `to` indexes
                // the folded stream, so applying markers as they arrive
                // is the same fold a full replay performs.
                SessionEvent::Rewind { to, .. } => {
                    self.events.truncate(to);
                    updates.push(TailUpdate::Rewound { line, to, event });
                }
                event => {
                    self.events.push(event.clone());
                    updates.push(TailUpdate::Appended { line, event });
                }
            }
        }
        self.line += lines;
        self.offset += committed.len() as u64;
        Ok(updates)
    }

    /// Rebuild the whole view from byte 0, stopping after `upto`
    /// physical lines when asked.
    fn replay(&mut self, upto: Option<usize>) -> std::io::Result<()> {
        let bytes = std::fs::read(&self.path)?;
        let committed = committed_len(&bytes);
        let end = match upto {
            None => committed,
            Some(lines) => nth_line_end(&bytes[..committed], lines).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "session {}: cannot resume at line {lines}, only {} committed",
                        self.id,
                        committed_line_count(&bytes[..committed])
                    ),
                )
            })?,
        };
        let prefix = &bytes[..end];
        self.events = fold_rewinds(parse_event_bytes(prefix, &self.id, 0)?);
        self.line = committed_line_count(prefix);
        self.offset = end as u64;
        Ok(())
    }
}

/// Device and inode, so a replaced file is not mistaken for an appended
/// one. Constant for a session's life in this store (the writer appends
/// in place and never renames), so a change means someone else acted.
type FileIdentity = (u64, u64);

#[cfg(unix)]
fn file_identity(metadata: &std::fs::Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev(), metadata.ino())
}

#[cfg(not(unix))]
fn file_identity(_metadata: &std::fs::Metadata) -> FileIdentity {
    (0, 0)
}

/// Byte offset just past the `count`-th newline, or `None` when the
/// slice holds fewer lines than that.
fn nth_line_end(bytes: &[u8], count: usize) -> Option<usize> {
    if count == 0 {
        return Some(0);
    }
    bytes
        .iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'\n')
        .nth(count - 1)
        .map(|(index, _)| index + 1)
}

/// Read `len` bytes from `offset`, opening the file for just this read:
/// no fd outlives a poll, so nothing here can pin a deleted session or
/// go stale against the path.
fn read_at(path: &Path, offset: u64, len: u64) -> std::io::Result<Vec<u8>> {
    let capacity = usize::try_from(len).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "session tail does not fit this platform",
        )
    })?;
    let mut file = std::fs::File::open(path)?;
    file.seek(std::io::SeekFrom::Start(offset))?;
    let mut buffer = Vec::with_capacity(capacity);
    file.take(len).read_to_end(&mut buffer)?;
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::event::{SessionMeta, new_id};
    use crate::session::model::{ContentBlock, Usage};
    use crate::session::store::Session;

    fn temp_store() -> (SessionStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        (store, dir)
    }

    /// A real session with its real writer: every mutation in these
    /// tests goes through the store, so the fixture exercises the true
    /// append, rewind, compaction and repair paths.
    fn start(store: &SessionStore) -> (String, Session) {
        let id = new_id();
        let session = store
            .create(SessionMeta {
                session_id: id.clone(),
                parent_id: None,
                agent: "build".into(),
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

    fn append_raw(path: &Path, bytes: &[u8]) {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(bytes).unwrap();
        file.sync_data().unwrap();
    }

    fn lines_of(updates: &[TailUpdate]) -> Vec<usize> {
        updates
            .iter()
            .map(|update| match update {
                TailUpdate::Appended { line, .. } | TailUpdate::Rewound { line, .. } => *line,
                other => panic!("expected a line-bearing update, got {other:?}"),
            })
            .collect()
    }

    /// The whole point: a line is reported once, when it is complete,
    /// and never again.
    #[test]
    fn every_append_is_delivered_exactly_once() {
        let (store, _dir) = temp_store();
        let (id, mut session) = start(&store);
        let mut tail = SessionTail::open(&store, &id).unwrap();

        assert_eq!(lines_of(&tail.poll().unwrap()), [1], "the meta line");
        assert!(tail.poll().unwrap().is_empty(), "nothing new to report");

        session.append(user("one")).unwrap();
        assert_eq!(lines_of(&tail.poll().unwrap()), [2]);

        session.append(assistant("two")).unwrap();
        session.append(user("three")).unwrap();
        assert_eq!(lines_of(&tail.poll().unwrap()), [3, 4], "batched, in order");
        assert!(tail.poll().unwrap().is_empty());
        assert_eq!(tail.line(), 4);

        drop(session);
        assert_eq!(tail.events(), store.audit_events(&id).unwrap());
    }

    /// A half-written line is not an event yet. It is held back whole,
    /// and the offset stays where the last newline was.
    #[test]
    fn a_torn_tail_is_held_back_then_delivered_whole() {
        let (store, _dir) = temp_store();
        let (id, mut session) = start(&store);
        session.append(user("committed")).unwrap();
        drop(session);
        let path = store.session_path(&id).unwrap();

        let mut tail = SessionTail::open(&store, &id).unwrap();
        assert_eq!(lines_of(&tail.poll().unwrap()), [1, 2]);

        let line = {
            let mut text = serde_json::to_string(&user("torn")).unwrap();
            text.push('\n');
            text.into_bytes()
        };
        let (head, rest) = line.split_at(line.len() / 2);
        append_raw(&path, head);
        assert!(tail.poll().unwrap().is_empty(), "no newline, no event");
        assert_eq!(tail.line(), 2, "the reader did not consume torn bytes");

        append_raw(&path, rest);
        let updates = tail.poll().unwrap();
        assert_eq!(lines_of(&updates), [3]);
        let TailUpdate::Appended { event, .. } = &updates[0] else {
            panic!("expected an append, got {updates:?}");
        };
        assert!(matches!(event, SessionEvent::UserMessage { text, .. } if text == "torn"));
    }

    /// A rewind marker is an append like any other; the fold it names is
    /// applied on arrival, and lands the reader on the store's own view.
    #[test]
    fn a_rewind_folds_the_view_and_matches_the_store() {
        let (store, _dir) = temp_store();
        let (id, mut session) = start(&store);
        session.append(user("first")).unwrap();
        session.append(assistant("did first")).unwrap();
        session.append(user("second")).unwrap();
        session.append(assistant("did second")).unwrap();

        let mut tail = SessionTail::open(&store, &id).unwrap();
        assert_eq!(lines_of(&tail.poll().unwrap()), [1, 2, 3, 4, 5]);

        session.rewind_to(3, None, None).unwrap();
        let updates = tail.poll().unwrap();
        let [
            TailUpdate::Rewound {
                line: 6,
                to: 3,
                event,
            },
        ] = &updates[..]
        else {
            panic!("expected one rewind at line 6, got {updates:?}");
        };
        assert!(
            matches!(event, SessionEvent::Rewind { to: 3, .. }),
            "the marker itself travels with the fold: {event:?}"
        );
        assert_eq!(tail.line(), 6, "the marker is a physical line");
        assert_eq!(tail.events().len(), 3);
        assert_eq!(tail.events(), store.load(&id).unwrap().events());
    }

    /// The subtle claim: `to` indexes the *folded* stream, so a second
    /// marker cuts a stream the first already shortened. Applying markers
    /// one at a time has to agree with a full replay's fold.
    #[test]
    fn two_rewinds_fold_the_same_way_a_full_replay_does() {
        let (store, _dir) = temp_store();
        let (id, mut session) = start(&store);
        session.append(user("first")).unwrap();
        session.append(assistant("did first")).unwrap();
        session.append(user("second")).unwrap();
        session.append(assistant("did second")).unwrap();

        let mut tail = SessionTail::open(&store, &id).unwrap();
        assert_eq!(tail.poll().unwrap().len(), 5);

        // Back to "second", then a new turn, then back past both.
        session.rewind_to(3, None, None).unwrap();
        let mut session = store.acquire_writer(&id).unwrap().load().unwrap();
        session.append(user("third")).unwrap();
        session.append(assistant("did third")).unwrap();
        session.rewind_to(1, None, None).unwrap();

        let updates = tail.poll().unwrap();
        assert_eq!(lines_of(&updates), [6, 7, 8, 9]);
        assert!(matches!(updates[0], TailUpdate::Rewound { to: 3, .. }));
        assert!(matches!(updates[3], TailUpdate::Rewound { to: 1, .. }));
        assert_eq!(tail.events().len(), 1, "only the meta line survives");
        let mut fresh = SessionTail::open(&store, &id).unwrap();
        fresh.poll().unwrap();
        assert_eq!(
            tail.events(),
            fresh.events(),
            "an incremental fold equals a fold from scratch"
        );
        assert_eq!(tail.events(), store.load(&id).unwrap().events());
    }

    /// The writer's torn-tail repair only ever removes bytes no reader
    /// committed, so it is invisible: no resync, no lost place.
    #[test]
    fn writer_tail_repair_is_invisible_to_the_reader() {
        let (store, _dir) = temp_store();
        let (id, mut session) = start(&store);
        session.append(user("committed")).unwrap();
        drop(session);
        let path = store.session_path(&id).unwrap();

        let mut tail = SessionTail::open(&store, &id).unwrap();
        assert_eq!(lines_of(&tail.poll().unwrap()), [1, 2]);

        append_raw(&path, b"{\"type\":\"user_message\",\"id\":\"tor");
        assert!(tail.poll().unwrap().is_empty());

        // Loading through the writer repairs the tail.
        let mut session = store.acquire_writer(&id).unwrap().load().unwrap();
        assert!(tail.poll().unwrap().is_empty(), "no resync was needed");
        assert_eq!(tail.line(), 2);

        session.append(user("after repair")).unwrap();
        assert_eq!(lines_of(&tail.poll().unwrap()), [3]);
        assert_eq!(tail.events().len(), 3);
    }

    /// Defence in depth: P5 says committed bytes are never truncated, so
    /// a file shorter than the reader's offset means the reader's whole
    /// view is suspect — it is rebuilt rather than patched.
    #[test]
    fn a_file_shorter_than_the_offset_resyncs() {
        let (store, _dir) = temp_store();
        let (id, mut session) = start(&store);
        session.append(user("first")).unwrap();
        session.append(assistant("did first")).unwrap();
        drop(session);
        let path = store.session_path(&id).unwrap();

        let mut tail = SessionTail::open(&store, &id).unwrap();
        assert_eq!(lines_of(&tail.poll().unwrap()), [1, 2, 3]);

        let bytes = std::fs::read(&path).unwrap();
        let cut = nth_line_end(&bytes, 2).unwrap() as u64;
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(cut)
            .unwrap();

        assert_eq!(tail.poll().unwrap(), vec![TailUpdate::Resync]);
        assert_eq!(tail.line(), 2);
        assert_eq!(tail.events(), store.audit_events(&id).unwrap());
        assert!(tail.poll().unwrap().is_empty());
    }

    /// A committed line that will not parse fails the whole slab: an
    /// audit log never skips a record, so the reader stays exactly where
    /// it was and the next poll meets the same wall.
    #[test]
    fn a_corrupt_committed_line_leaves_the_reader_untouched() {
        let (store, _dir) = temp_store();
        let (id, mut session) = start(&store);
        session.append(user("committed")).unwrap();
        drop(session);
        let path = store.session_path(&id).unwrap();

        let mut tail = SessionTail::open(&store, &id).unwrap();
        assert_eq!(lines_of(&tail.poll().unwrap()), [1, 2]);
        let before = tail.events().to_vec();

        // One good line and one broken one, committed together.
        let good = format!("{}\n", serde_json::to_string(&user("good")).unwrap());
        append_raw(&path, format!("{good}{{\"type\":\"nope\"}}\n").as_bytes());

        let error = tail.poll().unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("line 4"), "{error}");
        assert_eq!(tail.line(), 2, "no half-consumed slab");
        assert_eq!(tail.events(), before);
        assert_eq!(
            tail.poll().unwrap_err().kind(),
            std::io::ErrorKind::InvalidData,
            "the wall does not move"
        );
    }

    /// A blank line is not an event, but it is still a line: the physical
    /// numbering the wire ids ride on must not drift.
    #[test]
    fn a_blank_line_costs_a_line_number_but_yields_no_event() {
        let (store, _dir) = temp_store();
        let (id, session) = start(&store);
        drop(session);
        let path = store.session_path(&id).unwrap();

        let mut tail = SessionTail::open(&store, &id).unwrap();
        assert_eq!(lines_of(&tail.poll().unwrap()), [1]);

        append_raw(
            &path,
            format!("\n{}\n", serde_json::to_string(&user("after")).unwrap()).as_bytes(),
        );
        assert_eq!(lines_of(&tail.poll().unwrap()), [3], "line 2 was blank");
        assert_eq!(tail.line(), 3);
        assert_eq!(tail.events().len(), 2);
    }

    /// P4 says the inode never changes under the store. If it does, the
    /// bytes the reader consumed belong to a different file.
    #[cfg(unix)]
    #[test]
    fn a_replaced_file_resyncs() {
        let (store, dir) = temp_store();
        let (id, mut session) = start(&store);
        session.append(user("first")).unwrap();
        drop(session);
        let path = store.session_path(&id).unwrap();

        let mut tail = SessionTail::open(&store, &id).unwrap();
        assert_eq!(lines_of(&tail.poll().unwrap()), [1, 2]);

        let replacement = dir.path().join("replacement");
        std::fs::copy(&path, &replacement).unwrap();
        append_raw(
            &replacement,
            format!("{}\n", serde_json::to_string(&user("elsewhere")).unwrap()).as_bytes(),
        );
        std::fs::rename(&replacement, &path).unwrap();

        assert_eq!(tail.poll().unwrap(), vec![TailUpdate::Resync]);
        assert_eq!(tail.line(), 3);
        assert_eq!(tail.events(), store.audit_events(&id).unwrap());
    }

    #[test]
    fn a_deleted_session_reports_deleted_once() {
        let (store, _dir) = temp_store();
        let (id, session) = start(&store);
        drop(session);

        let mut tail = SessionTail::open(&store, &id).unwrap();
        assert_eq!(lines_of(&tail.poll().unwrap()), [1]);

        store.delete(&id).unwrap();
        assert_eq!(tail.poll().unwrap(), vec![TailUpdate::Deleted]);
        assert!(tail.poll().unwrap().is_empty(), "deletion is terminal");
    }

    #[test]
    fn open_at_resumes_mid_file() {
        let (store, _dir) = temp_store();
        let (id, mut session) = start(&store);
        session.append(user("first")).unwrap();
        session.append(assistant("did first")).unwrap();
        session.append(user("second")).unwrap();
        drop(session);
        let all = store.audit_events(&id).unwrap();

        let mut tail = SessionTail::open_at(&store, &id, 2).unwrap();
        assert_eq!(tail.line(), 2);
        assert_eq!(tail.events(), &all[..2]);

        let updates = tail.poll().unwrap();
        assert_eq!(
            lines_of(&updates),
            [3, 4],
            "resumes after the replayed lines"
        );
        assert_eq!(tail.events(), all);

        let error = SessionTail::open_at(&store, &id, 99).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    /// A compaction is a line in the log and nothing more to a reader:
    /// the window it names is a rendering decision, and the checkpoint
    /// it publishes is writer cache. The canary is the deleted sidecar —
    /// a reader that consulted it would fail here.
    #[test]
    fn compaction_is_a_plain_append_and_sidecars_are_never_read() {
        let (store, dir) = temp_store();
        let (id, mut session) = start(&store);
        session.append(user("first")).unwrap();
        session.append(assistant("did first")).unwrap();
        session.append(user("second")).unwrap();
        session.append(assistant("did second")).unwrap();
        session
            .append(SessionEvent::Compaction {
                id: new_id(),
                summary: "the story so far".into(),
                kept_from: 3,
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(session);

        let index_path = store.replay_index_path(&id).unwrap();
        assert!(index_path.exists(), "the writer published a checkpoint");
        std::fs::remove_file(&index_path).unwrap();
        for entry in std::fs::read_dir(dir.path()).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.contains(".replay.") {
                std::fs::remove_file(entry.path()).unwrap();
            }
        }

        let mut tail = SessionTail::open(&store, &id).unwrap();
        let updates = tail.poll().unwrap();
        assert_eq!(lines_of(&updates), [1, 2, 3, 4, 5, 6]);
        assert!(
            matches!(
                &updates[5],
                TailUpdate::Appended {
                    event: SessionEvent::Compaction { kept_from: 3, .. },
                    ..
                }
            ),
            "got {:?}",
            updates[5]
        );
        assert_eq!(
            tail.events(),
            store.audit_events(&id).unwrap(),
            "the compaction window is not applied to the reader's view"
        );
        assert!(!index_path.exists(), "the reader wrote nothing back");
    }

    /// The whole contract in one line: the folded tail is the audit log
    /// with the two-line client fold applied.
    #[test]
    fn the_folded_tail_equals_the_audit_log_folded_by_hand() {
        let (store, _dir) = temp_store();
        let (id, mut session) = start(&store);
        session.append(user("first")).unwrap();
        session.append(assistant("did first")).unwrap();
        session.append(user("second")).unwrap();
        session.append(assistant("did second")).unwrap();
        session.rewind_to(3, None, None).unwrap();

        let mut session = store.acquire_writer(&id).unwrap().load().unwrap();
        session.append(user("third")).unwrap();
        session.append(assistant("did third")).unwrap();
        session
            .append(SessionEvent::Compaction {
                id: new_id(),
                summary: "the story so far".into(),
                kept_from: 3,
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(session);

        let audit = store.audit_events(&id).unwrap();
        let mut folded: Vec<SessionEvent> = Vec::new();
        for event in audit.clone() {
            match event {
                SessionEvent::Rewind { to, .. } => folded.truncate(to),
                event => folded.push(event),
            }
        }

        let mut tail = SessionTail::open(&store, &id).unwrap();
        let updates = tail.poll().unwrap();
        assert_eq!(
            updates.len(),
            audit.len(),
            "one update per committed line, rewind markers included"
        );
        assert_eq!(tail.line(), audit.len());
        assert_eq!(tail.events(), folded);
    }
}
