//! history: search this session's own past, including what compaction
//! dropped.
//!
//! The context window is a working set, not the record. Everything ever
//! said in a session stays on disk, so a detail that fell out of context
//! is a query away rather than gone — which is what makes it safe to
//! compact hard.

use super::{
    Tool, ToolConcurrency, ToolContext, ToolFuture, ToolOutput, WorkspaceAccess, parse_input,
};
use crate::recall;
use crate::session::SessionStore;

/// Characters of any one entry shown when reading around a hit.
const CONTEXT_ENTRY_CHARS: usize = 400;
/// Events either side of a hit when reading around it.
const CONTEXT_RADIUS: usize = 2;
/// Rows one listing returns, and the characters they may add up to.
/// Every row is bounded on its own, but a long session's user messages
/// alone outgrow the window they were asked for; what does not fit is
/// named and reachable with `after`.
const MAX_LISTED: usize = 50;
const MAX_LISTING_CHARS: usize = 16_000;

#[derive(serde::Deserialize)]
struct Input {
    query: Option<String>,
    event: Option<u64>,
    speaker: Option<String>,
    /// Listing only: continue after this event index.
    after: Option<u64>,
}

pub struct HistoryTool {
    store: SessionStore,
}

impl HistoryTool {
    pub fn new(store: SessionStore) -> Self {
        Self { store }
    }
}

fn render_matches(matches: &[recall::Match], query: &str) -> String {
    if matches.is_empty() {
        return format!("no earlier mention of {query:?} in this session");
    }
    let mut lines = vec![format!(
        "{} match(es) for {query:?}; read around one with event=<n>:",
        matches.len()
    )];
    for hit in matches {
        let lead = if hit.elided_before { "…" } else { "" };
        let tail = if hit.elided_after { "…" } else { "" };
        lines.push(format!(
            "event {} · {}: {lead}{}{tail}",
            hit.event,
            hit.speaker.label(),
            hit.excerpt
        ));
    }
    lines.join("\n")
}

fn render_context(entries: &[recall::Entry], event: usize) -> String {
    if entries.is_empty() {
        return format!("event {event} is outside this session");
    }
    entries
        .iter()
        .map(|entry| {
            format!(
                "event {} · {}: {}",
                entry.event,
                entry.speaker.label(),
                entry.text
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_listing(
    entries: &[recall::Entry],
    speaker: recall::Speaker,
    left: usize,
    after: Option<usize>,
) -> String {
    if entries.is_empty() {
        return match after {
            Some(after) => format!(
                "nothing from {} after event {after} in this session",
                speaker.label()
            ),
            None => format!("this session has nothing from {}", speaker.label()),
        };
    }
    let mut lines = vec![format!(
        "{} entr(ies) from {}, oldest first:",
        entries.len(),
        speaker.label()
    )];
    for entry in entries {
        lines.push(format!("event {}: {}", entry.event, entry.text));
    }
    if left > 0 {
        let last = entries.last().map(|entry| entry.event).unwrap_or(0);
        lines.push(format!(
            "({left} more not shown; pass after={last} for the next page, or use query to \
             narrow)"
        ));
    }
    lines.join("\n")
}

impl Tool for HistoryTool {
    fn name(&self) -> &'static str {
        "history"
    }

    fn description(&self) -> &'static str {
        "Search this session's own history, including everything summarized away by \
         compaction. Use it whenever a detail you need is not in front of you — an earlier \
         instruction, a file path, an error, a decision and its reasoning — instead of \
         guessing or asking the user to repeat themselves. Search with `query`, narrow with \
         `speaker`, read around a hit with `event`, or pass `speaker: \"user\"` alone to list \
         every instruction you have been given in this session."
    }

    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Concurrent
    }

    fn workspace_access(&self) -> WorkspaceAccess {
        WorkspaceAccess::None
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": ["string", "null"],
                    "description": "Text to look for, case-insensitive. Omit when reading around an event."
                },
                "event": {
                    "type": ["integer", "null"],
                    "description": "Event index from a search result; returns the conversation around it."
                },
                "speaker": {
                    "type": ["string", "null"],
                    "enum": ["user", "assistant", "thinking", "tool_call", "tool_result", "summary", "topic", null],
                    "description": "Narrow a search to one speaker, or list everything one said when there is no query. `speaker: \"user\"` alone lists the instructions you were given."
                },
                "after": {
                    "type": ["integer", "null"],
                    "description": "Listing only: continue after this event index, as the end of a truncated listing tells you to."
                }
            }
        })
    }

    fn run(&self, input: serde_json::Value, ctx: ToolContext) -> ToolFuture {
        let store = self.store.clone();
        Box::pin(async move {
            // Typed, so a field of the wrong shape is a type error and
            // not a silent fall-through into another mode.
            let Input {
                query,
                event,
                speaker: speaker_word,
                after,
            } = match parse_input(input, "history") {
                Ok(input) => input,
                Err(error) => return error,
            };
            let speaker = match speaker_word.as_deref() {
                None => None,
                Some(word) => match recall::parse_speaker(word) {
                    Some(speaker) => Some(speaker),
                    None => {
                        return ToolOutput::error(format!(
                            "history: unknown speaker {word:?}; use user, assistant, thinking, \
                             tool_call, tool_result, summary or topic"
                        ));
                    }
                },
            };
            if ctx.session_id.is_empty() {
                return ToolOutput::error("history: available only inside a session");
            }
            // Its own session only, matching the resume guard: no
            // session reads another's log.
            //
            // The archive is read on the blocking pool: a long session
            // is megabytes of JSONL, and parsing it on a runtime worker
            // stalls every other task that worker was holding. The
            // scan stops on its own if this call is abandoned.
            let session_id = ctx.session_id.clone();
            let read = super::blocking_scan(move |cancelled| {
                recall::session_entries_until(&store, &session_id, &cancelled)
            })
            .await;
            let entries = match read {
                Ok(Ok(entries)) => entries,
                Ok(Err(error)) => return ToolOutput::error(format!("history: {error}")),
                Err(error) => {
                    return ToolOutput::error(format!("history: reading the session: {error}"));
                }
            };
            match (query, event) {
                (_, Some(event)) => {
                    let event = event as usize;
                    let around =
                        recall::around(&entries, event, CONTEXT_RADIUS, CONTEXT_ENTRY_CHARS);
                    ToolOutput::text(render_context(&around, event))
                }
                (Some(query), None) => {
                    let matches = recall::search(&entries, &query, speaker, recall::MAX_MATCHES);
                    ToolOutput::text(render_matches(&matches, &query))
                }
                // No query, just a speaker: list what they said. The
                // usual case is "what was I actually asked?".
                (None, None) => match speaker {
                    Some(speaker) => {
                        let listed = recall::by_speaker(&entries, speaker, CONTEXT_ENTRY_CHARS);
                        let after = after.map(|after| after as usize);
                        let (page, left) =
                            recall::page(&listed, after, MAX_LISTED, MAX_LISTING_CHARS);
                        ToolOutput::text(render_listing(&page, speaker, left, after))
                    }
                    None => ToolOutput::error("history: needs a query, an event, or a speaker"),
                },
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_render_as_addressable_rows() {
        let matches = vec![recall::Match {
            event: 42,
            speaker: recall::Speaker::ToolResult,
            excerpt: "the AES table lives here".into(),
            elided_before: true,
            elided_after: false,
        }];

        let rendered = render_matches(&matches, "aes table");

        assert!(
            rendered.contains("event 42 · tool result: …the AES table"),
            "{rendered}"
        );
        assert!(
            rendered.contains("event=<n>"),
            "no way to read further: {rendered}"
        );
        assert!(
            render_matches(&[], "nothing").contains("no earlier mention"),
            "empty result is not an error"
        );
    }

    fn said(event: usize, text: &str) -> recall::Entry {
        recall::Entry {
            event,
            speaker: recall::Speaker::User,
            text: text.into(),
        }
    }

    /// A listing that does not fit says so, and says how to see the
    /// rest — a truncated answer the model cannot tell is truncated is
    /// worse than a short one.
    #[test]
    fn a_listing_that_is_cut_says_how_to_go_on() {
        let entries: Vec<recall::Entry> =
            (1..=6).map(|n| said(n * 10, &format!("ask {n}"))).collect();

        let (page, left) = recall::page(&entries, None, 2, MAX_LISTING_CHARS);
        assert_eq!(left, 4);
        let rendered = render_listing(&page, recall::Speaker::User, left, None);
        assert!(rendered.contains("event 10: ask 1"), "{rendered}");
        assert!(rendered.contains("event 20: ask 2"), "{rendered}");
        assert!(!rendered.contains("ask 3"), "{rendered}");
        assert!(
            rendered.contains("(4 more not shown; pass after=20"),
            "{rendered}"
        );

        // The next page continues where that one stopped.
        let (next, left) = recall::page(&entries, Some(20), 2, MAX_LISTING_CHARS);
        assert_eq!(left, 2);
        assert_eq!(next.first().map(|entry| entry.event), Some(30));

        // The aggregate cap bites before the row cap when rows are big,
        // and one row always travels however long it is.
        let long = vec![said(1, &"x".repeat(500)), said(2, "short")];
        let (page, left) = recall::page(&long, None, 50, 100);
        assert_eq!((page.len(), left), (1, 1), "one row still travels");

        // A page past the end says so without pretending it is empty.
        let (none, left) = recall::page(&entries, Some(60), 50, MAX_LISTING_CHARS);
        assert_eq!((none.len(), left), (0, 0));
        let rendered = render_listing(&none, recall::Speaker::User, left, Some(60));
        assert!(rendered.contains("after event 60"), "{rendered}");
    }
}
