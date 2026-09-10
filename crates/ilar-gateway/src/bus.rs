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

/// Cut a long text into pieces a channel will show whole: at most
/// `max_chars` characters and `max_lines` lines each, split at line
/// boundaries where possible; a single line longer than `max_chars`
/// is cut where it must be.
pub fn split_for_delivery(text: &str, max_chars: usize, max_lines: usize) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut current = String::new();
    let mut lines = 0;
    for line in text.split_inclusive('\n') {
        let over =
            current.chars().count() + line.chars().count() > max_chars || lines + 1 > max_lines;
        if over && !current.is_empty() {
            pieces.push(std::mem::take(&mut current));
            lines = 0;
        }
        let mut line = line;
        while line.chars().count() > max_chars {
            let cut = line
                .char_indices()
                .nth(max_chars)
                .map(|(i, _)| i)
                .unwrap_or(line.len());
            pieces.push(line[..cut].to_string());
            line = &line[cut..];
        }
        current.push_str(line);
        lines += 1;
    }
    if !current.trim().is_empty() {
        pieces.push(current);
    }
    pieces
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
    fn long_texts_split_at_lines_and_only_cut_a_line_when_they_must() {
        assert_eq!(split_for_delivery("short", 10, 10), vec!["short"]);
        let text = "one\ntwo\nthree\n";
        assert_eq!(
            split_for_delivery(text, 8, 10),
            vec!["one\ntwo\n", "three\n"]
        );
        assert_eq!(
            split_for_delivery("abcdefghij", 4, 10),
            vec!["abcd", "efgh", "ij"]
        );
        assert!(split_for_delivery("", 4, 10).is_empty());
        // A line cap too: many short lines fold a bubble as surely as
        // a long one.
        assert_eq!(
            split_for_delivery("a\nb\nc\nd\ne\n", 100, 2),
            vec!["a\nb\n", "c\nd\n", "e\n"]
        );
    }

    #[test]
    fn keys_split_at_the_first_colon() {
        assert_eq!(session_key("deltachat", "12"), "deltachat:12");
        assert_eq!(split_key("deltachat:a:b"), Some(("deltachat", "a:b")));
        assert_eq!(split_key("bare"), None);
    }
}
