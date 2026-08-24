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

impl Tool for HistoryTool {
    fn name(&self) -> &'static str {
        "history"
    }

    fn description(&self) -> &'static str {
        "Search this session's own history, including everything summarized away by \
         compaction. Use it whenever a detail you need is not in front of you — an earlier \
         instruction, a file path, an error, a decision and its reasoning — instead of \
         guessing or asking the user to repeat themselves. Search with `query`; read the \
         surrounding conversation with `event` from a result."
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
                    let matches = recall::search(&entries, &query, recall::MAX_MATCHES);
                    ToolOutput::text(render_matches(&matches, &query))
                }
                (None, None) => ToolOutput::error("history needs a query or an event"),
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
