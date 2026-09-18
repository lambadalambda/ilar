//! Which session a chat is, and which chat was last heard from.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Routes {
    /// Session key → session id.
    pub sessions: BTreeMap<String, String>,
    /// Where an unaddressed message (the inbox, a cron job) goes.
    pub last_active: Option<String>,
    /// The same, counting private chats only: what the assistant knows
    /// about its person is not for a room, so a job the gateway owns
    /// reads this instead.
    #[serde(default)]
    pub last_private: Option<String>,
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
        if !is_group {
            self.last_private = Some(key.to_string());
        }
    }

    /// The last chat heard from that is not a room: where a job of the
    /// gateway's own — the weekly review, which reads the person's
    /// memory aloud — is allowed to speak.
    pub fn last_private_chat(&self) -> Option<&str> {
        self.last_private
            .as_deref()
            // Routes written before this field existed name no private
            // chat: the last active one stands in, as long as it is
            // not a room.
            .or(self.last_active.as_deref())
            .filter(|key| !self.is_group(key))
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

/// Write through a rename; the core's, shared by every file the
/// gateway keeps under its home.
pub(crate) use ilar::memory::write_atomically;

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
        // A room is heard from, but it is not where the weekly review
        // speaks: that is the last private chat.
        assert_eq!(reopened.last_private_chat(), Some("fake:1"));
        // A chat that turns out to be a room stops being one, rather
        // than standing as the last private chat on a stale flag.
        let mut routes = reopened;
        routes.touch("fake:g2", false);
        routes.touch("fake:g2", true);
        assert_eq!(routes.last_private_chat(), None);
        assert!(Routes::default().last_private_chat().is_none());
        // Routes from before the field existed: the last active chat
        // stands in for it, unless it is a room.
        let older = Routes {
            last_active: Some("fake:1".into()),
            ..Routes::default()
        };
        assert_eq!(older.last_private_chat(), Some("fake:1"));
        let older_room = Routes {
            last_active: Some("fake:g".into()),
            groups: vec!["fake:g".into()],
            ..Routes::default()
        };
        assert_eq!(older_room.last_private_chat(), None);
    }
}
