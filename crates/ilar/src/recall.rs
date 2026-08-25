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
/// something anyone remembers reading. Image payloads are skipped
/// deliberately, on user and tool-result events alike: base64 is not
/// text anyone searches, and indexing it would bury real hits.
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
                        ContentBlock::Image { .. } => {}
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

/// Carry a byte offset in `text.to_lowercase()` back to a byte offset
/// in `text` itself, in one walk and with nothing allocated.
///
/// The two strings only line up while every character lowercases to its
/// own byte length; 'İ' lowercases to "i\u{307}" and everything after
/// it has shifted. An offset landing inside such an expansion resolves
/// to the character holding it, so the answer is always a real char
/// boundary of `text` — the whole point, since the caller slices there.
fn original_offset(text: &str, lowered_at: usize) -> usize {
    let mut lowered = 0;
    for (index, character) in text.char_indices() {
        lowered += character.to_lowercase().map(char::len_utf8).sum::<usize>();
        if lowered > lowered_at {
            return index;
        }
    }
    text.len()
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
        let lowered = entry.text.to_lowercase();
        let Some(found) = lowered.find(&needle) else {
            continue;
        };
        // Both ends are indices into the lowercased text, a different
        // string: lowercasing can change a character's byte length
        // ('İ' becomes "i\u{307}"). Carry them back to the original
        // before anything slices it.
        let at = original_offset(&entry.text, found);
        let end = original_offset(&entry.text, found + needle.len());
        let (excerpt, elided_before, elided_after) =
            excerpt(&entry.text, at, end.saturating_sub(at));
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

/// Every hit one session produced for a query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionHits {
    pub session_id: String,
    /// Topic when the session has one, opening message otherwise —
    /// whatever the listing would call it.
    pub title: Option<String>,
    /// When the session was last written, for showing an age.
    pub modified: std::time::SystemTime,
    pub hits: Vec<Match>,
}

/// Search every root session's full history for `query`, newest session
/// first, calling `emit` once per session that matched. `emit` returning
/// `false` abandons the walk — the caller typed another key.
///
/// Sessions are read one at a time, so results stream in listing order
/// rather than arriving after the whole store has been scanned. The
/// session's entries ride along so a caller can build context around a
/// hit (via [`around`]) without reading the session a second time.
pub fn search_sessions<F: FnMut(&[Entry], SessionHits) -> bool>(
    store: &SessionStore,
    query: &str,
    per_session: usize,
    mut emit: F,
) {
    if query.trim().is_empty() {
        return;
    }
    for summary in store.list() {
        // A session that vanished or went unreadable mid-walk is
        // skipped, same as the listing would.
        let Ok(entries) = session_entries(store, &summary.id) else {
            continue;
        };
        let hits = search(&entries, query, None, per_session);
        if hits.is_empty() {
            continue;
        }
        let keep_going = emit(
            &entries,
            SessionHits {
                session_id: summary.id,
                title: summary.title,
                modified: summary.modified,
                hits,
            },
        );
        if !keep_going {
            return;
        }
    }
}

/// Walk every root session newest-first with no query at all: one
/// pseudo-hit per session, anchored at its last entry and excerpting
/// that entry's tail — the session shown by its last words. This is
/// the search modal's empty-query listing; an empty query matches
/// everything, fzf-style.
pub fn tail_sessions<F: FnMut(&[Entry], SessionHits) -> bool>(store: &SessionStore, mut emit: F) {
    for summary in store.list() {
        let Ok(entries) = session_entries(store, &summary.id) else {
            continue;
        };
        let Some(last) = entries.last() else {
            continue;
        };
        let (excerpt, elided_before, _) = excerpt(&last.text, last.text.len(), 0);
        let hit = Match {
            event: last.event,
            speaker: last.speaker,
            excerpt,
            elided_before,
            elided_after: false,
        };
        let keep_going = emit(
            &entries,
            SessionHits {
                session_id: summary.id,
                title: summary.title,
                modified: summary.modified,
                hits: vec![hit],
            },
        );
        if !keep_going {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SessionEvent, new_id};

    fn user(text: &str) -> SessionEvent {
        SessionEvent::UserMessage {
            id: new_id(),
            text: text.into(),
            images: Vec::new(),
            ts: chrono::Utc::now(),
        }
    }

    fn result(text: &str) -> SessionEvent {
        SessionEvent::ToolResult {
            id: new_id(),
            tool_use_id: "call-1".into(),
            content: text.into(),
            is_error: false,
            images: Vec::new(),
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
    fn a_dotted_capital_i_does_not_slice_a_character_in_half() {
        // 'İ' lowercases to two chars ("i\u{307}"), so any byte index
        // taken from the lowercased text lands mid-character in the
        // original — the excerpt must never be cut there.
        let events = vec![user("İstanbul was the last stop")];

        let matches = search(&entries(&events), "i", None, MAX_MATCHES);

        assert_eq!(matches.len(), 1);
        assert!(matches[0].excerpt.contains("İstanbul"), "{matches:?}");

        // A query starting inside that expansion — the combining dot
        // the 'İ' lowercased into — has no case-matching position in
        // the original at all, and used to fall back to the lowercased
        // index against the original text.
        let matches = search(&entries(&events), "\u{307}stanbul", None, MAX_MATCHES);

        assert_eq!(matches.len(), 1);
        assert!(matches[0].excerpt.contains("İstanbul"), "{matches:?}");
    }

    #[test]
    fn a_lengthening_char_before_a_hit_still_excerpts_the_term() {
        let events = vec![user("İzmir first, then the AES table")];

        let matches = search(&entries(&events), "aes table", None, MAX_MATCHES);

        assert_eq!(matches.len(), 1);
        assert!(matches[0].excerpt.contains("AES table"), "{matches:?}");
    }

    #[test]
    fn an_ascii_hit_is_windowed_from_the_match_itself() {
        let left = "a".repeat(200);
        let right = "b".repeat(200);
        let events = vec![result(&format!("{left}TARGET{right}"))];

        let matches = search(&entries(&events), "target", None, MAX_MATCHES);

        assert_eq!(matches.len(), 1);
        // Exactly EXCERPT_RADIUS characters either side of the hit:
        // an offset off by one would shift the whole window.
        assert_eq!(
            matches[0].excerpt,
            format!(
                "{}TARGET{}",
                "a".repeat(EXCERPT_RADIUS),
                "b".repeat(EXCERPT_RADIUS)
            )
        );
        assert!(matches[0].elided_before && matches[0].elided_after);
    }

    #[test]
    fn an_empty_query_matches_nothing() {
        let entries = entries(&[user("anything")]);
        assert!(search(&entries, "   ", None, MAX_MATCHES).is_empty());
    }
}
