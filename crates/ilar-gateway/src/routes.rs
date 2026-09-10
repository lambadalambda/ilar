//! Which session a chat is, and which chat was last heard from.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Routes {
    /// Session key → session id.
    pub sessions: BTreeMap<String, String>,
    /// Where an unaddressed message (the inbox, a cron job) goes.
    pub last_active: Option<String>,
    /// Keys that are group chats.
    #[serde(default)]
    pub groups: Vec<String>,
    /// Cron and heartbeat sessions, by their keys: remembered so a
    /// restart resumes them, kept apart so nothing can address them.
    #[serde(default)]
    pub background: BTreeMap<String, String>,
}

impl Routes {
    /// The chats that have written, for a refusal to name.
    pub fn known_chats(&self) -> String {
        if self.sessions.is_empty() {
            "(none yet)".to_string()
        } else {
            self.sessions
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        }
    }

    pub fn session_for(&self, key: &str) -> Option<&str> {
        self.sessions.get(key).map(String::as_str)
    }

    pub fn bind(&mut self, key: &str, session_id: &str) {
        self.sessions
            .insert(key.to_string(), session_id.to_string());
    }

    /// Forget a chat's session: the next message starts a new one.
    pub fn unbind(&mut self, key: &str) {
        self.sessions.remove(key);
    }

    pub fn touch(&mut self, key: &str, is_group: bool) {
        self.last_active = Some(key.to_string());
        let listed = self.groups.iter().any(|group| group == key);
        if is_group && !listed {
            self.groups.push(key.to_string());
        }
    }

    pub fn is_group(&self, key: &str) -> bool {
        self.groups.iter().any(|group| group == key)
    }

    pub fn background_session_for(&self, key: &str) -> Option<&str> {
        self.background.get(key).map(String::as_str)
    }

    pub fn bind_background(&mut self, key: &str, session_id: &str) {
        self.background
            .insert(key.to_string(), session_id.to_string());
    }
}

/// The routes on disk: one JSON file, rewritten whole through a rename.
pub struct RouteStore {
    path: PathBuf,
    routes: Mutex<Routes>,
}

impl RouteStore {
    pub fn open(path: PathBuf) -> anyhow::Result<Self> {
        let routes = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("parsing routes {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Routes::default(),
            Err(error) => return Err(error).context("reading routes"),
        };
        Ok(Self {
            path,
            routes: Mutex::new(routes),
        })
    }

    pub fn snapshot(&self) -> Routes {
        self.routes.lock().unwrap().clone()
    }

    /// Change and persist in one step; the change is applied whether or
    /// not the write succeeds, and the write's error is the caller's.
    pub fn update(&self, change: impl FnOnce(&mut Routes)) -> anyhow::Result<()> {
        let mut routes = self.routes.lock().unwrap();
        change(&mut routes);
        write_atomically(&self.path, &serde_json::to_vec_pretty(&*routes)?)
    }
}

/// Write through a rename. The temporary name is unique per write, so
/// two writers racing on one file both land — the later one wins —
/// instead of one failing on a name the other just renamed away.
pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let serial = SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp.{}.{serial}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_bind_touch_and_survive_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("routes.json");
        let store = RouteStore::open(path.clone()).unwrap();
        store
            .update(|routes| {
                routes.bind("fake:1", "s1");
                routes.touch("fake:1", false);
                routes.touch("fake:g", true);
            })
            .unwrap();
        store
            .update(|routes| routes.bind_background("heartbeat:fake:1", "b1"))
            .unwrap();
        let reopened = RouteStore::open(path).unwrap().snapshot();
        assert_eq!(reopened.session_for("fake:1"), Some("s1"));
        assert_eq!(reopened.session_for("heartbeat:fake:1"), None);
        assert_eq!(
            reopened.background_session_for("heartbeat:fake:1"),
            Some("b1")
        );
        assert_eq!(reopened.last_active.as_deref(), Some("fake:g"));
        assert!(reopened.is_group("fake:g"));
        assert!(!reopened.is_group("fake:1"));
    }
}
