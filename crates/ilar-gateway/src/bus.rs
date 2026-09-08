//! What travels between a channel and the agent.

use std::path::PathBuf;

/// A message a person sent through a channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inbound {
    pub channel: String,
    pub chat_id: String,
    pub sender_id: String,
    pub text: String,
    /// Attachments, as files the channel already fetched.
    pub media: Vec<PathBuf>,
    /// A group is not a private chat: what memory is injected, and who
    /// the model thinks it is talking to, both read this.
    pub is_group: bool,
}

impl Inbound {
    pub fn session_key(&self) -> String {
        session_key(&self.channel, &self.chat_id)
    }
}

/// A message the agent sends through a channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outbound {
    pub channel: String,
    pub chat_id: String,
    pub text: String,
    pub media: Vec<PathBuf>,
}

/// `<channel>:<chat>` names a session. picoclaw's convention, kept so
/// the background keys (`cron:…`, `heartbeat:…`) read the same way.
pub fn session_key(channel: &str, chat_id: &str) -> String {
    format!("{channel}:{chat_id}")
}

/// The two halves of a key. A chat id may itself contain colons, so the
/// split is at the first one.
pub fn split_key(key: &str) -> Option<(&str, &str)> {
    key.split_once(':')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_split_at_the_first_colon() {
        assert_eq!(session_key("deltachat", "12"), "deltachat:12");
        assert_eq!(split_key("deltachat:a:b"), Some(("deltachat", "a:b")));
        assert_eq!(split_key("bare"), None);
    }
}
