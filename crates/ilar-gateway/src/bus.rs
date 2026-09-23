//! What travels between a channel and the agent.

use std::path::PathBuf;

/// A message a person sent through a channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inbound {
    pub channel: String,
    pub chat_id: String,
    pub sender_id: String,
    /// What the sender is called, as the channel shows it. In a room
    /// the model is told who spoke; in a private chat it is not
    /// needed, and a message the gateway made up has none.
    pub sender_name: Option<String>,
    /// The channel's own id for this message, when it has one: what
    /// deleting it again needs. `None` for a message the gateway made
    /// up, from the inbox.
    pub message_id: Option<String>,
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
    /// Answers the person can tap, where the channel has buttons: each
    /// is a command, handled as if the tapping person had typed it.
    /// The text names the commands too, for a channel that has none.
    pub buttons: Vec<Button>,
}

/// One tappable answer under a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Button {
    /// What the button says.
    pub label: String,
    /// The command a tap sends, slash and all: `/grant session`.
    pub command: String,
}

impl Button {
    pub fn new(label: &str, command: &str) -> Self {
        Self {
            label: label.to_string(),
            command: command.to_string(),
        }
    }
}

/// `<channel>:<chat>` names a session. picoclaw's convention, kept so
/// the background keys (`cron:…`, `heartbeat:…`) read the same way.
pub fn session_key(channel: &str, chat_id: &str) -> String {
    format!("{channel}:{chat_id}")
}

/// Cut a long text into pieces a channel will show whole, counting
/// the way Delta Chat's `truncate_by_lines` does: a line of `line_len`
/// characters is one display line, a longer one is several, a blank
/// line is one. Pieces stay under `max_lines` display lines and split
/// at line breaks; a single line too long for a piece is cut at a
/// space where it can be.
pub fn split_for_delivery(text: &str, max_lines: usize, line_len: usize) -> Vec<String> {
    let max_lines = max_lines.max(1);
    let line_len = line_len.max(1);
    let display_lines = |line: &str| line.chars().count().max(1).div_ceil(line_len);
    let mut pieces = Vec::new();
    let mut current = String::new();
    let mut used = 0;
    for line in text.split_inclusive('\n') {
        let mut line = line;
        // A line that cannot fit a piece of its own is cut at spaces.
        while display_lines(line.trim_end_matches('\n')) > max_lines {
            let limit = max_lines * line_len;
            let hard = line
                .char_indices()
                .nth(limit)
                .map(|(i, _)| i)
                .unwrap_or(line.len());
            let cut = line[..hard].rfind(' ').map(|i| i + 1).unwrap_or(hard);
            if !current.is_empty() {
                pieces.push(std::mem::take(&mut current));
                used = 0;
            }
            pieces.push(line[..cut].to_string());
            line = &line[cut..];
        }
        let needed = display_lines(line.trim_end_matches('\n'));
        if used + needed > max_lines && !current.is_empty() {
            pieces.push(std::mem::take(&mut current));
            used = 0;
        }
        current.push_str(line);
        used += needed;
    }
    if !current.trim().is_empty() {
        pieces.push(current);
    }
    pieces
}

/// Cut a long text into pieces of at most `max_units` UTF-16 units,
/// newlines included, for a channel that caps a message that way rather
/// than by what it folds — Telegram's 4096, in which an emoji outside the
/// basic plane counts twice. Pieces split at line breaks; a single line
/// too long for a piece is cut at a space where it can be. A piece that
/// would be only whitespace is dropped: Telegram refuses an empty text.
pub fn split_by_chars(text: &str, max_units: usize) -> Vec<String> {
    let max_units = max_units.max(2);
    let units = |text: &str| text.chars().map(char::len_utf16).sum::<usize>();
    let mut pieces = Vec::new();
    let mut current = String::new();
    let mut used = 0;
    for line in text.split_inclusive('\n') {
        let mut line = line;
        while units(line) > max_units {
            // The byte where `max_units` runs out, at a char boundary.
            let mut seen = 0;
            let hard = line
                .char_indices()
                .find(|(_, c)| {
                    seen += c.len_utf16();
                    seen > max_units
                })
                .map(|(i, _)| i)
                .unwrap_or(line.len());
            let cut = match line[..hard].rfind(' ') {
                Some(space) if space > 0 => space + 1,
                _ => hard,
            };
            if !current.is_empty() {
                pieces.push(std::mem::take(&mut current));
                used = 0;
            }
            pieces.push(line[..cut].to_string());
            line = &line[cut..];
        }
        let needed = units(line);
        if used + needed > max_units && !current.is_empty() {
            pieces.push(std::mem::take(&mut current));
            used = 0;
        }
        current.push_str(line);
        used += needed;
    }
    pieces.push(current);
    pieces.retain(|piece| !piece.trim().is_empty());
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

    /// A list of forty short lines is one Telegram message, not two:
    /// the cap is characters, and every piece stays under it.
    #[test]
    fn a_character_capped_channel_splits_by_characters() {
        let list: String = (0..40).map(|i| format!("- item {i}\n")).collect();
        assert_eq!(split_by_chars(&list, 4000), vec![list.clone()]);
        let long: String = (0..600).map(|i| format!("line number {i}\n")).collect();
        let pieces = split_by_chars(&long, 4000);
        assert!(pieces.len() > 1);
        assert!(pieces.iter().all(|piece| piece.chars().count() <= 4000));
        assert_eq!(pieces.concat(), long, "nothing lost or doubled");
        let one_line = "word ".repeat(1000);
        let pieces = split_by_chars(&one_line, 4000);
        assert!(pieces.iter().all(|piece| piece.chars().count() <= 4000));
        assert_eq!(pieces.concat(), one_line);
        // Telegram counts UTF-16 units: an emoji outside the basic plane
        // is two, and 3000 of them are 6000 units, not 3000.
        let emoji = "😀".repeat(3000);
        let pieces = split_by_chars(&emoji, 4000);
        assert!(pieces.len() > 1);
        assert!(
            pieces
                .iter()
                .all(|piece| piece.chars().map(char::len_utf16).sum::<usize>() <= 4000)
        );
        // Never a piece of nothing but whitespace, which Telegram refuses.
        let blob = format!("\n\n\n{}", "x".repeat(5000));
        assert!(
            split_by_chars(&blob, 4000)
                .iter()
                .all(|p| !p.trim().is_empty())
        );
    }

    #[test]
    fn long_texts_split_by_display_lines_the_way_delta_chat_counts() {
        assert_eq!(split_for_delivery("short", 10, 100), vec!["short"]);
        // Three one-line paragraphs, two per piece.
        let text = "one\ntwo\nthree\n";
        assert_eq!(
            split_for_delivery(text, 2, 100),
            vec!["one\ntwo\n", "three\n"]
        );
        // A 250-character line is three display lines of 100.
        let long = "x".repeat(250);
        assert_eq!(split_for_delivery(&long, 3, 100), vec![long.clone()]);
        assert_eq!(
            split_for_delivery(&format!("{long}\n{long}\n"), 3, 100).len(),
            2
        );
        // A line too long for any piece is cut at a space.
        let words = "word ".repeat(100);
        let pieces = split_for_delivery(&words, 2, 100);
        assert!(pieces.len() >= 3, "{pieces:?}");
        assert!(
            pieces.iter().all(|p| p.chars().count() <= 200),
            "{pieces:?}"
        );
        assert!(pieces[0].ends_with(' '), "{pieces:?}");
        assert!(split_for_delivery("", 4, 100).is_empty());
        // The shape that folded: paragraphs of 600-1,100 characters with
        // blank lines between, 38 display lines in 3,600 characters.
        let essay = [599, 661, 1089, 471, 401, 1097, 629]
            .iter()
            .map(|n| "w".repeat(*n))
            .collect::<Vec<_>>()
            .join("\n\n");
        let pieces = split_for_delivery(&essay, 34, 100);
        assert!(pieces.len() >= 2, "{}", pieces.len());
        for piece in &pieces {
            let lines: usize = piece
                .split_inclusive('\n')
                .map(|l| {
                    l.trim_end_matches('\n')
                        .chars()
                        .count()
                        .max(1)
                        .div_ceil(100)
                })
                .sum();
            assert!(lines <= 34, "{lines}");
        }
    }

    #[test]
    fn keys_split_at_the_first_colon() {
        assert_eq!(session_key("deltachat", "12"), "deltachat:12");
        assert_eq!(split_key("deltachat:a:b"), Some(("deltachat", "a:b")));
        assert_eq!(split_key("bare"), None);
    }
}
