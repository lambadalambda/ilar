//! What the agent is doing right now, in one line: folded from the
//! loop's events for a status message the chat can watch.

use std::collections::HashMap;

use ilar::agent::{LoopEvent, summarize_tool_input};

/// The line shown before anything has happened.
pub const WORKING: &str = "working…";

const MAX_CHARS: usize = 90;

/// Folds events into the current status; says when it changed.
#[derive(Default)]
pub struct Narrator {
    tools: HashMap<String, String>,
    summary: String,
    current: String,
}

impl Narrator {
    /// The new status line, when this event changed it.
    pub fn observe(&mut self, event: &LoopEvent) -> Option<String> {
        let next = match event {
            LoopEvent::ReasoningSummaryDelta(delta) => {
                self.summary.push_str(delta);
                match heading(&self.summary) {
                    Some(topic) => format!("thinking — {topic}"),
                    None => "thinking…".to_string(),
                }
            }
            LoopEvent::ReasoningSummaryCompleted => {
                self.summary.clear();
                return None;
            }
            LoopEvent::ToolStarted { id, name } => {
                self.tools.insert(id.clone(), name.clone());
                format!("calling {name}…")
            }
            LoopEvent::ToolInputComplete { id, arguments } => {
                let name = self.tools.get(id).cloned().unwrap_or_default();
                let input: serde_json::Value = serde_json::from_str(arguments).unwrap_or_default();
                let summary = summarize_tool_input(&name, &input);
                if summary.trim().is_empty() {
                    format!("running {name}")
                } else {
                    format!("running {name}: {summary}")
                }
            }
            LoopEvent::SubagentConfigured {
                description, agent, ..
            } => format!("delegating to {agent}: {description}"),
            LoopEvent::TextDelta(_) => "writing…".to_string(),
            LoopEvent::Compacted { .. } => "compacting the conversation…".to_string(),
            LoopEvent::ProviderRetry {
                attempt,
                max_retries,
                ..
            } => format!("the provider stumbled; retry {attempt} of {max_retries}…"),
            _ => return None,
        };
        let next = clip(&next);
        if next == self.current {
            return None;
        }
        self.current = next.clone();
        Some(next)
    }
}

/// The bold heading a reasoning summary opens with, or its first line.
fn heading(summary: &str) -> Option<String> {
    let text = summary.trim_start();
    if let Some(rest) = text.strip_prefix("**")
        && let Some(end) = rest.find("**")
    {
        let topic = rest[..end].trim();
        return (!topic.is_empty()).then(|| topic.to_string());
    }
    let line = text.lines().next()?.trim().trim_matches('*').trim();
    (!line.is_empty() && text.contains('\n')).then(|| line.to_string())
}

fn clip(text: &str) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= MAX_CHARS {
        return flat;
    }
    let mut cut: String = flat.chars().take(MAX_CHARS - 1).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_become_a_status_line_that_only_reports_changes() {
        let mut narrator = Narrator::default();
        assert_eq!(
            narrator.observe(&LoopEvent::ReasoningSummaryDelta("**Planning".into())),
            Some("thinking…".into())
        );
        assert_eq!(
            narrator.observe(&LoopEvent::ReasoningSummaryDelta(
                " the fix**\n\nFirst".into()
            )),
            Some("thinking — Planning the fix".into())
        );
        assert_eq!(
            narrator.observe(&LoopEvent::ReasoningSummaryDelta(" more".into())),
            None
        );
        assert_eq!(
            narrator.observe(&LoopEvent::ReasoningSummaryCompleted),
            None
        );
        assert_eq!(
            narrator.observe(&LoopEvent::ToolStarted {
                id: "1".into(),
                name: "bash".into()
            }),
            Some("calling bash…".into())
        );
        let running = narrator
            .observe(&LoopEvent::ToolInputComplete {
                id: "1".into(),
                arguments: r#"{"command": "cargo test -p ilar"}"#.into(),
            })
            .unwrap();
        assert!(running.starts_with("running bash: "), "{running}");
        assert!(running.contains("cargo test"), "{running}");
        assert_eq!(narrator.observe(&LoopEvent::TurnStarted), None);
        assert_eq!(
            narrator.observe(&LoopEvent::TextDelta("x".into())),
            Some("writing…".into())
        );
        assert_eq!(narrator.observe(&LoopEvent::TextDelta("y".into())), None);
        let long = "a".repeat(300);
        let clipped = narrator
            .observe(&LoopEvent::ReasoningSummaryDelta(format!("**{long}**\n")))
            .unwrap();
        assert!(clipped.chars().count() <= MAX_CHARS, "{clipped}");
        assert!(clipped.ends_with('…'));
    }
}
