//! Searching a session's own history.
//!
//! A session's log outlives its context window: compaction drops
//! material from the transcript, but every line stays on disk. This is
//! the walk over that archive — one scanner behind two front doors, the
//! model's `history` tool and the user's cross-session search.
//!
//! What it returns are *excerpts*, never whole events. A search that
//! hands back the 100k-character hexdump it matched has recreated the
//! problem it exists to solve.

use crate::session::{ContentBlock, SessionEvent, SessionStore};

/// Characters of context shown on each side of a match.
const EXCERPT_RADIUS: usize = 120;
/// Most matches returned for one query.
pub const MAX_MATCHES: usize = 40;

/// Where a line came from and what it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speaker {
    User,
    Assistant,
    Thinking,
    ToolCall,
    ToolResult,
    Summary,
    Topic,
}

impl Speaker {
    pub fn label(self) -> &'static str {
        match self {
            Speaker::User => "user",
            Speaker::Assistant => "assistant",
            Speaker::Thinking => "thinking",
            Speaker::ToolCall => "tool call",
            Speaker::ToolResult => "tool result",
            Speaker::Summary => "summary",
            Speaker::Topic => "topic",
        }
    }
}

/// One searchable piece of a session, addressed by the event it came
/// from so a caller can ask for its neighbours.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Index into the session's canonical event list.
    pub event: usize,
    pub speaker: Speaker,
    pub text: String,
}

/// A hit, with just enough of its surroundings to judge it by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub event: usize,
    pub speaker: Speaker,
    /// The matched text with a bounded window on either side.
    pub excerpt: String,
    /// Whether text was dropped before or after the excerpt.
    pub elided_before: bool,
    pub elided_after: bool,
}

/// Flatten a session's events into searchable text, in order.
///
/// Tool calls contribute their arguments and tool results their output:
/// both are places a half-remembered detail actually lives. Provider
/// reasoning blobs and replay items do not — they are machine state, not
/// something anyone remembers reading.
pub fn entries(events: &[SessionEvent]) -> Vec<Entry> {
    let mut entries = Vec::new();
    for (index, event) in events.iter().enumerate() {
        let mut push = |speaker: Speaker, text: &str| {
            if !text.trim().is_empty() {
                entries.push(Entry {
                    event: index,
                    speaker,
                    text: text.to_string(),
                });
            }
        };
        match event {
            SessionEvent::UserMessage { text, .. } => push(Speaker::User, text),
            SessionEvent::Topic { text, .. } => push(Speaker::Topic, text),
            SessionEvent::Compaction { summary, .. } => push(Speaker::Summary, summary),
            SessionEvent::ToolResult { content, .. } => push(Speaker::ToolResult, content),
            SessionEvent::AssistantMessage { content, .. } => {
                for block in content {
                    match block {
                        ContentBlock::Text { text } => push(Speaker::Assistant, text),
                        ContentBlock::Thinking { text, .. } => push(Speaker::Thinking, text),
                        ContentBlock::ToolCall { name, input, .. } => {
                            push(Speaker::ToolCall, &format!("{name} {input}"));
                        }
                        ContentBlock::ToolResult { content, .. } => {
                            push(Speaker::ToolResult, content);
                        }
                        ContentBlock::ReasoningSummary { text, .. } => {
                            push(Speaker::Thinking, text);
                        }
                        ContentBlock::Reasoning { .. }
                        | ContentBlock::ProviderReplay { .. }
                        | ContentBlock::Diagnostic { .. } => {}
                    }
                }
            }
            SessionEvent::Meta { .. }
            | SessionEvent::SubagentInvocation { .. }
            | SessionEvent::Checkpoint { .. }
            | SessionEvent::ModelChange { .. }
            | SessionEvent::Rewind { .. } => {}
        }
    }
    entries
}

/// Cut a bounded window around `at`, on character boundaries, widened
/// to whitespace where one is near so words are not sliced in half.
fn excerpt(text: &str, at: usize, needle_len: usize) -> (String, bool, bool) {
    let start = text[..at]
        .char_indices()
        .rev()
        .take(EXCERPT_RADIUS)
        .last()
        .map(|(index, _)| index)
        .unwrap_or(0);
    let after = at + needle_len;
    let end = text[after.min(text.len())..]
        .char_indices()
        .take(EXCERPT_RADIUS)
        .last()
        .map(|(index, character)| after + index + character.len_utf8())
        .unwrap_or(after.min(text.len()));
    let slice = text[start..end].trim();
    (
        slice.split_whitespace().collect::<Vec<_>>().join(" "),
        start > 0,
        end < text.len(),
    )
}

/// Parse a speaker filter from a caller's word, e.g. the `history`
/// tool's `speaker` argument. `None` means every speaker.
pub fn parse_speaker(word: &str) -> Option<Speaker> {
    match word.trim().to_lowercase().replace([' ', '-'], "_").as_str() {
        "user" => Some(Speaker::User),
        "assistant" => Some(Speaker::Assistant),
        "thinking" => Some(Speaker::Thinking),
        "tool_call" => Some(Speaker::ToolCall),
        "tool_result" => Some(Speaker::ToolResult),
        "summary" => Some(Speaker::Summary),
        "topic" => Some(Speaker::Topic),
        _ => None,
    }
}

/// Everything one speaker said, newest last, each bounded. Listing the
/// user's own messages answers "what was I actually asked?" without a
/// query — which is why the handover does not need to carry the request
/// verbatim.
pub fn by_speaker(entries: &[Entry], speaker: Speaker, max_chars: usize) -> Vec<Entry> {
    entries
        .iter()
        .filter(|entry| entry.speaker == speaker)
        .map(|entry| Entry {
            event: entry.event,
            speaker: entry.speaker,
            text: bound(&entry.text, max_chars),
        })
        .collect()
}

/// Case-insensitive substring search over a session's own history,
/// optionally narrowed to one speaker.
///
/// One match per entry: a query that appears fifty times in one hexdump
/// should cost one row, not fifty.
pub fn search(
    entries: &[Entry],
    query: &str,
    speaker: Option<Speaker>,
    limit: usize,
) -> Vec<Match> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    let mut matches = Vec::new();
    for entry in entries {
        if speaker.is_some_and(|wanted| wanted != entry.speaker) {
            continue;
        }
        let Some(at) = entry.text.to_lowercase().find(&needle) else {
            continue;
        };
        // The lowercase index is a byte index into a different string;
        // only ASCII-safe when the prefix is ASCII, so re-find on the
        // original where the case matches, and fall back to the start.
        let at = entry
            .text
            .char_indices()
            .map(|(index, _)| index)
            .find(|index| {
                entry.text[*index..]
                    .to_lowercase()
                    .starts_with(needle.as_str())
            })
            .unwrap_or(at.min(entry.text.len()));
        let (excerpt, elided_before, elided_after) = excerpt(&entry.text, at, needle.len());
        matches.push(Match {
            event: entry.event,
            speaker: entry.speaker,
            excerpt,
            elided_before,
            elided_after,
        });
        if matches.len() >= limit {
            break;
        }
    }
    matches
}

/// Everything said in events `event ± radius`, for reading around a
/// hit. Entries are bounded individually so one enormous tool result
/// cannot flood the answer.
pub fn around(entries: &[Entry], event: usize, radius: usize, max_chars: usize) -> Vec<Entry> {
    let low = event.saturating_sub(radius);
    let high = event.saturating_add(radius);
    entries
        .iter()
        .filter(|entry| entry.event >= low && entry.event <= high)
        .map(|entry| Entry {
            event: entry.event,
            speaker: entry.speaker,
            text: bound(&entry.text, max_chars),
        })
        .collect()
}

fn bound(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let head: String = text.chars().take(max_chars).collect();
    format!("{head}…")
}

/// Read a session's full canonical history — everything, including what
/// compaction dropped from the transcript.
pub fn session_entries(store: &SessionStore, session_id: &str) -> std::io::Result<Vec<Entry>> {
    Ok(entries(&store.audit_events(session_id)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SessionEvent, new_id};

    fn user(text: &str) -> SessionEvent {
        SessionEvent::UserMessage {
            id: new_id(),
            text: text.into(),
            ts: chrono::Utc::now(),
        }
    }

    fn result(text: &str) -> SessionEvent {
        SessionEvent::ToolResult {
            id: new_id(),
            tool_use_id: "call-1".into(),
            content: text.into(),
            is_error: false,
            child_session_id: None,
            state: None,
            ts: chrono::Utc::now(),
        }
    }

    #[test]
    fn every_place_a_detail_hides_is_searchable() {
        let events = vec![
            user("start with the GM1 firmware"),
            SessionEvent::AssistantMessage {
                id: new_id(),
                model: "zai/glm-4.7".into(),
                content: vec![
                    ContentBlock::Text {
                        text: "reading the container".into(),
                    },
                    ContentBlock::ToolCall {
                        id: "call-1".into(),
                        name: "bash".into(),
                        input: serde_json::json!({"command": "xxd fixtures/GM1.bin"}),
                        item_id: None,
                    },
                    // Machine state nobody remembers reading.
                    ContentBlock::Reasoning {
                        item: serde_json::json!({"encrypted_content": "SECRET"}),
                    },
                ],
                usage: Default::default(),
                stop_reason: "tool_use".into(),
                ts: chrono::Utc::now(),
            },
            result("offset 0x4f11b4 holds the AES table"),
        ];

        let entries = entries(&events);
        let text = format!("{entries:?}");
        assert!(text.contains("GM1 firmware"), "{text}");
        assert!(text.contains("reading the container"), "{text}");
        assert!(text.contains("xxd fixtures"), "user tool arguments: {text}");
        assert!(text.contains("0x4f11b4"), "tool output: {text}");
        assert!(
            !text.contains("SECRET"),
            "provider reasoning leaked: {text}"
        );
        // Entries carry the event they came from, so a hit is addressable.
        assert_eq!(entries.last().unwrap().event, 2);
        assert_eq!(entries[0].speaker, Speaker::User);
    }

    #[test]
    fn a_hit_comes_back_bounded_and_addressed() {
        let noise = "x".repeat(5_000);
        let events = vec![result(&format!("{noise} the AES table lives here {noise}"))];
        let entries = entries(&events);

        let matches = search(&entries, "aes table", None, MAX_MATCHES);

        assert_eq!(matches.len(), 1);
        let hit = &matches[0];
        assert_eq!(hit.event, 0);
        assert!(hit.excerpt.contains("AES table"), "{hit:?}");
        // Bounded: the 10k of noise around it does not come along.
        assert!(hit.excerpt.chars().count() < EXCERPT_RADIUS * 3, "{hit:?}");
        assert!(hit.elided_before && hit.elided_after, "{hit:?}");
    }

    #[test]
    fn one_row_per_entry_however_often_it_matches() {
        let events = vec![result(&"deadbeef ".repeat(500))];
        let matches = search(&entries(&events), "deadbeef", None, MAX_MATCHES);
        assert_eq!(matches.len(), 1, "a hexdump billed one row per byte");
    }

    #[test]
    fn reading_around_a_hit_stays_bounded() {
        let events = vec![
            user("first"),
            result(&"y".repeat(10_000)),
            user("third"),
            user("fourth"),
        ];
        let entries = entries(&events);

        let context = around(&entries, 1, 1, 100);

        assert_eq!(context.len(), 3, "{context:?}");
        assert_eq!(context[0].text, "first");
        assert!(context[1].text.chars().count() <= 101, "{:?}", context[1]);
        assert!(context[1].text.ends_with('…'));
        assert_eq!(context[2].text, "third");
        // Clamped at both ends of the log.
        assert_eq!(around(&entries, 0, 5, 100).len(), entries.len());
    }

    #[test]
    fn an_empty_query_matches_nothing() {
        let entries = entries(&[user("anything")]);
        assert!(search(&entries, "   ", None, MAX_MATCHES).is_empty());
    }
}
