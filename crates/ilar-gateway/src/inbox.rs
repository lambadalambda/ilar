//! `ilar-gateway notify`: a message from a script, a CI job, another
//! process, dropped as a file and picked up by the running gateway.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// What a message from a script is signed with, so the gateway can
/// tell it from a person typing: a script's text is never a command.
pub const SENDER_PREFIX: &str = "notify:";

/// How a script's message is signed, for the sender field.
pub fn sender(source: &str) -> String {
    format!("{SENDER_PREFIX}{source}")
}

/// Whether a sender is a script rather than a person.
pub fn is_script(sender_id: &str) -> bool {
    sender_id.starts_with(SENDER_PREFIX)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InboxMessage {
    pub source: String,
    pub text: String,
    /// A session key; the last active chat when absent.
    pub to: Option<String>,
}

/// Drop one message. The name sorts by arrival; the write is a rename
/// so the poller never reads half a file.
pub fn write(dir: &Path, message: &InboxMessage) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let stamp = chrono::Utc::now().timestamp_micros();
    let safe: String = message
        .source
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .take(32)
        .collect();
    let path = dir.join(format!("{stamp}-{safe}.json"));
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec(message)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Every message waiting, oldest first, each with the file to remove
/// once it has been handled. An unreadable file is skipped, not fatal.
pub fn drain(dir: &Path) -> anyhow::Result<Vec<(PathBuf, InboxMessage)>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("reading inbox"),
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    Ok(paths
        .into_iter()
        .filter_map(|path| {
            let message = std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())?;
            Some((path, message))
        })
        .collect())
}

/// One message per source per interval; the rest are dropped, since a
/// script in a loop is exactly what this exists to contain.
pub struct RateLimit {
    interval: Duration,
    last: HashMap<String, Instant>,
}

impl RateLimit {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: HashMap::new(),
        }
    }

    pub fn admit(&mut self, source: &str, now: Instant) -> bool {
        match self.last.get(source) {
            Some(last) if now.duration_since(*last) < self.interval => false,
            _ => {
                self.last.insert(source.to_string(), now);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_written_then_drained_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let first = InboxMessage {
            source: "ci".into(),
            text: "one".into(),
            to: None,
        };
        let second = InboxMessage {
            source: "ci/job".into(),
            text: "two".into(),
            to: Some("fake:1".into()),
        };
        write(dir.path(), &first).unwrap();
        write(dir.path(), &second).unwrap();
        let drained = drain(dir.path()).unwrap();
        let texts: Vec<&str> = drained.iter().map(|(_, m)| m.text.as_str()).collect();
        assert_eq!(texts, ["one", "two"]);
        assert_eq!(drained[1].1, second);
        for (path, _) in &drained {
            std::fs::remove_file(path).unwrap();
        }
        assert!(drain(dir.path()).unwrap().is_empty());
        assert!(drain(&dir.path().join("absent")).unwrap().is_empty());
    }

    #[test]
    fn a_source_gets_one_message_per_interval() {
        let mut limit = RateLimit::new(Duration::from_secs(60));
        let start = Instant::now();
        assert!(limit.admit("ci", start));
        assert!(!limit.admit("ci", start + Duration::from_secs(30)));
        assert!(limit.admit("other", start + Duration::from_secs(30)));
        assert!(limit.admit("ci", start + Duration::from_secs(61)));
    }
}
