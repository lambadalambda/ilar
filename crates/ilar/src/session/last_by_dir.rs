//! The per-directory "last session here" pointer — see
//! meta/issues/sessions-list-fast-and-true.md.
//!
//! In almost every case the session somebody wants is the last one from
//! the directory they are standing in, and finding it used to mean
//! reading the head of every file in the sessions directory. So it is
//! written down: one small JSON file mapping a canonical launch
//! directory to the session last used there, rewritten whenever a root
//! session is created, resumed or laid to rest.
//!
//! Advisory, like the summary cache beside it. A pointer that names a
//! session which is gone, is somebody's subagent, or belongs to another
//! directory is simply not believed, and the caller falls back to the
//! listing and repairs it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const POINTER_VERSION: u32 = 1;

/// The pointer file's name in the sessions directory. Not a `.jsonl`,
/// so nothing that scans for session logs can see it.
const POINTER_NAME: &str = "last-by-dir.json";

pub(super) fn pointer_path(root: &Path) -> PathBuf {
    root.join(POINTER_NAME)
}

/// One directory's answer: the session, and the stamp its file had when
/// the answer was written. A file whose mtime has gone *backwards* is
/// not the file that was pointed at any more.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct Pointer {
    pub(super) session_id: String,
    pub(super) modified_nanos: u64,
}

/// Keyed by the canonical launch directory as a string. A directory
/// whose path is not UTF-8 gets no pointer and falls back to the
/// listing — one lost shortcut, not a lost session.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(super) struct Pointers {
    version: u32,
    #[serde(default)]
    entries: BTreeMap<String, Pointer>,
}

impl Pointers {
    pub(super) fn get(&self, dir: &Path) -> Option<&Pointer> {
        self.entries.get(dir.to_str()?)
    }

    /// Point `dir` at a session. Returns whether anything changed, so a
    /// caller can skip the write — the common case is pointing a
    /// directory at the session it already names.
    pub(super) fn set(&mut self, dir: &Path, pointer: Pointer) -> bool {
        let Some(key) = dir.to_str() else {
            return false;
        };
        if self.entries.get(key) == Some(&pointer) {
            return false;
        }
        self.entries.insert(key.to_string(), pointer);
        true
    }

    /// Drop every directory pointing at `session_id`: a session that
    /// was removed must not stay pointed at, and one session can only
    /// be the answer for one directory anyway.
    pub(super) fn forget(&mut self, session_id: &str) -> bool {
        let before = self.entries.len();
        self.entries
            .retain(|_, pointer| pointer.session_id != session_id);
        self.entries.len() != before
    }
}

/// The pointers on disk, or none. Every failure mode — no file, a torn
/// write, a version nobody here wrote — is the same answer.
pub(super) fn load(root: &Path) -> Pointers {
    std::fs::read(pointer_path(root))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Pointers>(&bytes).ok())
        .filter(|pointers| pointers.version == POINTER_VERSION)
        .unwrap_or_default()
}

/// Read, change, write — atomically, and only when `change` says
/// something moved. Returns whether the file was written. Two
/// processes can still lose each other's last write; what that costs
/// is one directory listing.
pub(super) fn update(root: &Path, change: impl FnOnce(&mut Pointers) -> bool) -> bool {
    let mut pointers = load(root);
    if !change(&mut pointers) {
        return false;
    }
    pointers.version = POINTER_VERSION;
    let Ok(bytes) = serde_json::to_vec(&pointers) else {
        return false;
    };
    crate::atomic_file::replace(
        &pointer_path(root),
        &bytes,
        crate::atomic_file::Mode::Force(0o600),
    )
    .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pointer(id: &str) -> Pointer {
        Pointer {
            session_id: id.into(),
            modified_nanos: 7,
        }
    }

    /// Set, replace, forget — and "nothing moved" is reported as such,
    /// because that is what saves the write.
    #[test]
    fn a_directory_has_one_answer_at_a_time() {
        let dir = Path::new("/work/project");
        let mut pointers = Pointers::default();

        assert!(pointers.set(dir, pointer("first")));
        assert!(
            !pointers.set(dir, pointer("first")),
            "pointing at the same session is not a change"
        );
        assert_eq!(pointers.get(dir).unwrap().session_id, "first");

        assert!(pointers.set(dir, pointer("second")));
        assert_eq!(pointers.get(dir).unwrap().session_id, "second");

        assert!(!pointers.forget("first"), "nothing named it any more");
        assert!(pointers.forget("second"));
        assert!(pointers.get(dir).is_none());
    }

    /// Two directories, two answers, and a round trip through the file.
    #[test]
    fn the_file_survives_a_round_trip() {
        let root = tempfile::tempdir().unwrap();
        update(root.path(), |pointers| {
            pointers.set(Path::new("/one"), pointer("a"));
            pointers.set(Path::new("/two"), pointer("b"))
        });

        let loaded = load(root.path());

        assert_eq!(loaded.get(Path::new("/one")).unwrap().session_id, "a");
        assert_eq!(loaded.get(Path::new("/two")).unwrap().session_id, "b");
    }

    /// A write that changes nothing does not touch the file — the
    /// common case, since a directory is usually pointed at the session
    /// it already names.
    #[test]
    fn an_unchanged_pointer_is_not_rewritten() {
        let root = tempfile::tempdir().unwrap();
        assert!(update(root.path(), |pointers| {
            pointers.set(Path::new("/one"), pointer("a"))
        }));

        assert!(!update(root.path(), |pointers| {
            pointers.set(Path::new("/one"), pointer("a"))
        }));
    }

    #[test]
    fn a_torn_pointer_file_reads_as_nothing_known() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(pointer_path(root.path()), b"{\"version\":1,\"entr").unwrap();
        assert!(load(root.path()).get(Path::new("/one")).is_none());
    }
}
