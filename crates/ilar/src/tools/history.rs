//! history: search this session's own past, including what compaction
//! dropped.
//!
//! The context window is a working set, not the record. Everything ever
//! said in a session stays on disk, so a detail that fell out of context
//! is a query away rather than gone — which is what makes it safe to
//! compact hard.

use super::{Tool, ToolConcurrency, ToolContext, ToolFuture, ToolOutput, WorkspaceAccess};
use crate::recall;
use crate::session::SessionStore;

/// Characters of any one entry shown when reading around a hit.
const CONTEXT_ENTRY_CHARS: usize = 400;
/// Events either side of a hit when reading around it.
const CONTEXT_RADIUS: usize = 2;

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

fn render_listing(entries: &[recall::Entry], speaker: recall::Speaker) -> String {
    if entries.is_empty() {
        return format!("this session has nothing from {}", speaker.label());
    }
    let mut lines = vec![format!(
        "{} entr(ies) from {}, oldest first:",
        entries.len(),
        speaker.label()
    )];
    for entry in entries {
        lines.push(format!("event {}: {}", entry.event, entry.text));
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
                }
            }
        })
    }

    fn run(&self, input: serde_json::Value, ctx: ToolContext) -> ToolFuture {
        let store = self.store.clone();
        Box::pin(async move {
            let query = input
                .get("query")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            let event = input.get("event").and_then(serde_json::Value::as_u64);
            let speaker_word = input
                .get("speaker")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            let speaker = match speaker_word.as_deref() {
                None => None,
                Some(word) => match recall::parse_speaker(word) {
                    Some(speaker) => Some(speaker),
                    None => {
                        return ToolOutput::error(format!(
                            "unknown speaker {word:?}; use user, assistant, thinking, tool_call, \
                             tool_result, summary or topic"
                        ));
                    }
                },
            };
            if ctx.session_id.is_empty() {
                return ToolOutput::error("history is available only inside a session");
            }
            // Its own session only, matching the resume guard: no
            // session reads another's log.
            let entries = match recall::session_entries(&store, &ctx.session_id) {
                Ok(entries) => entries,
                Err(error) => {
                    return ToolOutput::error(format!("reading session history: {error}"));
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
                        ToolOutput::text(render_listing(&listed, speaker))
                    }
                    None => ToolOutput::error("history needs a query, an event, or a speaker"),
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
}
