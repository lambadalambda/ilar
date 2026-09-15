//! The sessions directory's summary cache — see
//! meta/issues/sessions-list-fast-and-true.md.
//!
//! Listing sessions used to cost one open, a 256 KiB head read and a
//! tail read *per file in the directory*, subagent children included,
//! every time anything asked. The cache turns that into one JSON read
//! plus a head read for the files whose stamp moved: a session log is
//! append-only, so mtime and length together are a complete answer to
//! "is what I wrote down still true?".
//!
//! It is advisory. A missing, truncated, foreign or older-versioned
//! cache is ignored and rebuilt, and two processes listing at once just
//! write over each other's copy — the cost of losing the race is a
//! reread, never a wrong answer.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::store::SessionHead;

/// Bumped when the shape below changes; an older file is dropped
/// rather than migrated, because rebuilding it is a directory walk.
const CACHE_VERSION: u32 = 1;

/// The cache file's name in the sessions directory. Not a `.jsonl`, so
/// nothing that scans the directory for session logs can see it.
const CACHE_NAME: &str = "summaries.json";

pub(super) fn cache_path(root: &Path) -> PathBuf {
    root.join(CACHE_NAME)
}

/// What one session file turned out to be, once something read its
/// head. This is the whole point of the cache: the answer survives, so
/// the next listing does not open the file again.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum CachedKind {
    /// A session of its own: what a listing row needs, and nothing else.
    Root {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<PathBuf>,
    },
    /// A subagent's session, and whose it is. Listings hide children by
    /// construction, and remembering that is what lets them skip the
    /// file without opening it — 2,148 of 2,421 files, measured.
    Child { parent: String },
    /// A `.jsonl` named like a session whose head does not parse: a
    /// half-written log, or somebody else's file. Cached so it is not
    /// reopened on every listing either.
    Unreadable,
}

/// One remembered file: the stamp the answer was true for, and the
/// answer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct CachedSummary {
    len: u64,
    modified_nanos: u64,
    #[serde(flatten)]
    kind: CachedKind,
}

/// The file's contents. A `BTreeMap` so the serialization is
/// deterministic: "did anything change?" is then a comparison, not a
/// guess, and a listing that changed nothing writes nothing.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(super) struct SummaryCache {
    version: u32,
    #[serde(default)]
    entries: BTreeMap<String, CachedSummary>,
}

/// A session file as the directory entry describes it, before anything
/// opens it.
#[derive(Clone, Debug)]
pub(super) struct ScannedFile {
    pub(super) id: String,
    pub(super) path: PathBuf,
    pub(super) modified: std::time::SystemTime,
    pub(super) len: u64,
}

/// A session file with its head resolved — from the cache when the
/// stamp held, from the file when it did not.
#[derive(Clone, Debug)]
pub(super) struct Scanned {
    pub(super) id: String,
    pub(super) path: PathBuf,
    pub(super) modified: std::time::SystemTime,
    pub(super) kind: CachedKind,
}

/// The mtime as the cache records it. The same reduction
/// `replay_index::file_stamp` makes, and for the same reason: a
/// comparable integer that survives a round trip through JSON. Shared
/// with the directory pointer, which stamps its answers the same way.
pub(super) fn modified_nanos(modified: std::time::SystemTime) -> u64 {
    modified
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64
}

fn kind_of(head: Option<SessionHead>) -> CachedKind {
    match head {
        None => CachedKind::Unreadable,
        Some(head) => match head.meta.parent_id {
            Some(parent) => CachedKind::Child { parent },
            None => CachedKind::Root {
                title: head.title,
                cwd: head.meta.cwd,
            },
        },
    }
}

/// Resolve a directory listing against the cache, reading the head of
/// every file whose stamp moved and of nothing else.
///
/// Returns the resolved entries in the order they were given, the cache
/// as it should now be on disk, and whether that differs from what was
/// read — the caller writes only in the third case.
///
/// The fresh cache is built from the directory, not patched into the
/// old one, so files that were deleted drop out instead of accumulating
/// for ever.
pub(super) fn resolve<F>(
    files: Vec<ScannedFile>,
    cache: &SummaryCache,
    mut read_head: F,
) -> (Vec<Scanned>, SummaryCache, bool)
where
    F: FnMut(&ScannedFile) -> Option<SessionHead>,
{
    let usable = cache.version == CACHE_VERSION;
    let mut entries = BTreeMap::new();
    let mut scanned = Vec::with_capacity(files.len());
    for file in files {
        let modified_nanos = modified_nanos(file.modified);
        let hit = usable
            .then(|| cache.entries.get(&file.id))
            .flatten()
            .filter(|entry| entry.len == file.len && entry.modified_nanos == modified_nanos);
        let kind = match hit {
            Some(entry) => entry.kind.clone(),
            None => kind_of(read_head(&file)),
        };
        entries.insert(
            file.id.clone(),
            CachedSummary {
                len: file.len,
                modified_nanos,
                kind: kind.clone(),
            },
        );
        scanned.push(Scanned {
            id: file.id,
            path: file.path,
            modified: file.modified,
            kind,
        });
    }
    let changed = !usable || entries != cache.entries;
    (
        scanned,
        SummaryCache {
            version: CACHE_VERSION,
            entries,
        },
        changed,
    )
}

/// The cache on disk, or an empty one. Every failure mode — no file, a
/// torn write, a newer version, somebody else's JSON — is the same
/// answer: nothing is known yet.
pub(super) fn load(root: &Path) -> SummaryCache {
    std::fs::read(cache_path(root))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<SummaryCache>(&bytes).ok())
        .filter(|cache| cache.version == CACHE_VERSION)
        .unwrap_or_default()
}

/// Best-effort, atomic, and never an error the caller has to care
/// about: a sessions directory that cannot be written to still lists
/// perfectly well, it just pays for it every time.
pub(super) fn save(root: &Path, cache: &SummaryCache) {
    let Ok(bytes) = serde_json::to_vec(cache) else {
        return;
    };
    let _ = crate::atomic_file::replace(
        &cache_path(root),
        &bytes,
        crate::atomic_file::Mode::Force(0o600),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(id: &str, len: u64, nanos: u64) -> ScannedFile {
        ScannedFile {
            id: id.into(),
            path: PathBuf::from(format!("/sessions/{id}.jsonl")),
            modified: std::time::UNIX_EPOCH + std::time::Duration::from_nanos(nanos),
            len,
        }
    }

    fn head(id: &str, parent: Option<&str>, title: &str) -> SessionHead {
        SessionHead {
            id: id.into(),
            meta: super::super::event::SessionMeta {
                session_id: id.into(),
                parent_id: parent.map(str::to_string),
                agent: "build".into(),
                model: "test/model".into(),
                workspace: None,
                cwd: None,
            },
            title: Some(title.into()),
            modified: std::time::UNIX_EPOCH,
        }
    }

    /// The whole contract in one test: a cold pass opens every file, a
    /// warm pass opens none, and a file whose stamp moved — and only
    /// that file — is read again.
    #[test]
    fn only_a_moved_stamp_costs_a_read() {
        let files = vec![file("a", 10, 1), file("b", 20, 2)];
        let mut opened: Vec<String> = Vec::new();
        let (scanned, cache, changed) = resolve(files.clone(), &SummaryCache::default(), |file| {
            opened.push(file.id.clone());
            Some(head(&file.id, None, "first title"))
        });
        assert_eq!(opened, vec!["a", "b"], "a cold cache reads everything");
        assert!(changed, "a cold pass has something to write");
        assert_eq!(scanned.len(), 2);

        let mut opened: Vec<String> = Vec::new();
        let (scanned, _, changed) = resolve(files.clone(), &cache, |file| {
            opened.push(file.id.clone());
            Some(head(&file.id, None, "rewritten"))
        });
        assert!(opened.is_empty(), "a warm cache reopened {opened:?}");
        assert!(!changed, "nothing moved, so nothing is written");
        assert!(matches!(
            &scanned[0].kind,
            CachedKind::Root { title: Some(title), .. } if title == "first title"
        ));

        // `b` grew; `a` did not.
        let mut opened: Vec<String> = Vec::new();
        let (scanned, _, changed) =
            resolve(vec![file("a", 10, 1), file("b", 21, 3)], &cache, |file| {
                opened.push(file.id.clone());
                Some(head(&file.id, None, "rewritten"))
            });
        assert_eq!(opened, vec!["b"]);
        assert!(changed);
        assert!(matches!(
            &scanned[1].kind,
            CachedKind::Root { title: Some(title), .. } if title == "rewritten"
        ));
    }

    /// A child is remembered as one, so the next listing skips it
    /// without opening it — the bulk of the directory, measured.
    #[test]
    fn a_child_is_remembered_as_a_child() {
        let files = vec![file("kid", 10, 1)];
        let (_, cache, _) = resolve(files.clone(), &SummaryCache::default(), |file| {
            Some(head(&file.id, Some("parent"), "a task"))
        });

        let mut opened = 0;
        let (scanned, _, _) = resolve(files, &cache, |_| {
            opened += 1;
            None
        });

        assert_eq!(opened, 0, "a known child was opened again");
        assert!(matches!(
            &scanned[0].kind,
            CachedKind::Child { parent } if parent == "parent"
        ));
    }

    /// A file whose head will not parse is cached as such: a foreign or
    /// half-written `.jsonl` must not be reopened on every listing
    /// either.
    #[test]
    fn an_unreadable_head_is_not_retried_until_it_changes() {
        let files = vec![file("junk", 5, 1)];
        let (_, cache, _) = resolve(files.clone(), &SummaryCache::default(), |_| None);

        let mut opened = 0;
        let (scanned, _, changed) = resolve(files, &cache, |_| {
            opened += 1;
            None
        });

        assert_eq!(opened, 0);
        assert!(!changed);
        assert!(matches!(scanned[0].kind, CachedKind::Unreadable));
    }

    /// The cache is rebuilt from the directory, so a deleted session
    /// leaves nothing behind to grow for ever.
    #[test]
    fn a_vanished_session_drops_out_of_the_cache() {
        let (_, cache, _) = resolve(
            vec![file("a", 10, 1), file("gone", 10, 1)],
            &SummaryCache::default(),
            |file| Some(head(&file.id, None, "t")),
        );
        assert_eq!(cache.entries.len(), 2);

        let (_, cache, changed) = resolve(vec![file("a", 10, 1)], &cache, |_| {
            panic!("a surviving file was reread")
        });

        assert!(changed, "a removal is a change worth writing");
        assert_eq!(cache.entries.keys().collect::<Vec<_>>(), vec!["a"]);
    }

    /// A cache from another version is not migrated, it is ignored:
    /// rebuilding is a directory walk, and trusting a shape nobody
    /// wrote is worse.
    #[test]
    fn a_foreign_version_is_ignored_and_rebuilt() {
        let (_, mut cache, _) = resolve(vec![file("a", 10, 1)], &SummaryCache::default(), |file| {
            Some(head(&file.id, None, "t"))
        });
        cache.version = CACHE_VERSION + 7;

        let mut opened = 0;
        let (_, fresh, changed) = resolve(vec![file("a", 10, 1)], &cache, |file| {
            opened += 1;
            Some(head(&file.id, None, "t"))
        });

        assert_eq!(opened, 1);
        assert!(changed);
        assert_eq!(fresh.version, CACHE_VERSION);
    }

    /// A round trip through the file, including the enum's tag: a
    /// shape that does not survive serialization is a cache that never
    /// hits.
    #[test]
    fn the_file_survives_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let (_, cache, _) = resolve(
            vec![file("a", 10, 1), file("kid", 3, 2)],
            &SummaryCache::default(),
            |file| {
                Some(head(
                    &file.id,
                    (file.id == "kid").then_some("a"),
                    "the title",
                ))
            },
        );
        save(dir.path(), &cache);

        let loaded = load(dir.path());

        assert_eq!(loaded.entries, cache.entries);
        let (_, _, changed) = resolve(vec![file("a", 10, 1), file("kid", 3, 2)], &loaded, |_| {
            panic!("a round-tripped cache missed")
        });
        assert!(!changed);
    }

    /// Garbage in the cache file is not an error anybody sees.
    #[test]
    fn a_torn_cache_file_reads_as_nothing_known() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(cache_path(dir.path()), b"{\"version\":1,\"entr").unwrap();
        assert!(load(dir.path()).entries.is_empty());
    }
}
