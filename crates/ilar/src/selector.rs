//! Memory recall's selector: a small model reads one line per note and
//! picks the few that will help with the message, the way Claude Code's
//! recall does. A model matches on what a message is about; the word
//! ranking it replaces matched the session's topic and was turned off
//! for it (DEVLOG, 2026-10-02).

use chrono::{DateTime, Utc};
use futures::StreamExt as _;

use crate::memory::Note;
use crate::provider::{EventStream, ProviderEvent, Request, StopReason};
use crate::session::{ChatMessage, ContentBlock, Role};

/// How long a turn's first request waits for the selector. A later
/// answer joins at the next step, if the turn takes one.
pub const GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// The selector's text, read off its stream. `None` for a failure or
/// an end other than the model finishing: recall is best effort.
pub async fn answer(mut stream: EventStream) -> Option<String> {
    let mut text = String::new();
    while let Some(event) = stream.next().await {
        match event {
            ProviderEvent::TextDelta(delta) => text.push_str(&delta),
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::EndTurn,
                ..
            } => return Some(text),
            ProviderEvent::TurnComplete { .. }
            | ProviderEvent::Error(_)
            | ProviderEvent::RetryableError(_)
            | ProviderEvent::RateLimited { .. } => return None,
            _ => {}
        }
    }
    None
}

/// What the selector is told. Adapted from Claude Code's, for ilar's
/// note kinds.
pub const SELECTOR_PROMPT: &str = "You pick which memory notes will help a coding agent \
with the person's message. You get a numbered list of notes, one per line: the number, the \
note's kind, its age, its title and its summary. Then the message.\n\
Answer with the numbers of the notes that will clearly help with this message, at most five. \
Pick a note only when you are sure from its title and summary.\n\
- Match on what the message is about, not on words it shares with a note. A note about the \
project in general is not about every message in the project.\n\
- Be extra careful with preference and event notes: they say how the person works or what \
happened once, not what each message is about.\n\
- If no note clearly helps, answer with an empty list. That is a good answer.\n\
Answer with JSON only: {\"picks\": [numbers]}.";

/// The listing the selector reads: notes numbered from 1, one line
/// each, never the body. Numbers rather than ids, so a model that
/// copies badly picks a wrong note at worst, never one that is not
/// there.
pub fn listing(notes: &[&Note], now: DateTime<Utc>) -> String {
    notes
        .iter()
        .enumerate()
        .map(|(index, note)| {
            format!(
                "{}. [{}] {} — {}: {}\n",
                index + 1,
                note.kind,
                crate::memory::age(now, note.when),
                note.title,
                note.summary
            )
        })
        .collect()
}

/// The selector's request: the listing first, so it can be the cached
/// prefix (the turn gives it a cache key), then the message.
pub fn request(model: &str, listing: &str, message: &str, options: serde_json::Value) -> Request {
    Request {
        system_prompt: Some(SELECTOR_PROMPT.into()),
        messages: vec![ChatMessage {
            role: Role::User,
            content: vec![
                ContentBlock::text(format!("Notes:\n{listing}")),
                ContentBlock::text(format!("Message:\n{message}")),
            ],
        }],
        options,
        ..Request::with_model(model)
    }
}

/// The notes an answer picked, by position in the listing: out of
/// range, repeated or past `max` are dropped, and an answer that is
/// not the JSON asked for picks nothing.
pub fn picks(answer: &str, count: usize, max: usize) -> Vec<usize> {
    // Fenced or not, the object is what sits between the braces.
    let object = match (answer.find('{'), answer.rfind('}')) {
        (Some(start), Some(end)) if start < end => &answer[start..=end],
        _ => return Vec::new(),
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(object) else {
        return Vec::new();
    };
    let mut picked = Vec::new();
    for number in value["picks"].as_array().into_iter().flatten() {
        // A quoted number is still the number the model meant.
        let Some(index) = number
            .as_u64()
            .or_else(|| number.as_str()?.trim().parse().ok())
            .and_then(|n| usize::try_from(n).ok())
            .and_then(|n| n.checked_sub(1))
        else {
            continue;
        };
        if index < count && !picked.contains(&index) {
            picked.push(index);
        }
    }
    picked.truncate(max);
    picked
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(id: &str, kind: &str, title: &str, days: i64, now: DateTime<Utc>) -> Note {
        Note {
            id: id.into(),
            kind: kind.into(),
            title: title.into(),
            summary: format!("what {title} says"),
            when: now - chrono::Duration::days(days),
            body: "the body never reaches the selector".into(),
        }
    }

    #[test]
    fn the_listing_numbers_one_line_per_note_without_bodies() {
        let now = Utc::now();
        let (a, b) = (
            note("20261101-aaaa", "decision", "PPU timing", 0, now),
            note("20261001-bbbb", "preference", "Tests on tenco", 35, now),
        );
        assert_eq!(
            listing(&[&a, &b], now),
            "1. [decision] today — PPU timing: what PPU timing says\n\
             2. [preference] 1mo ago — Tests on tenco: what Tests on tenco says\n"
        );
    }

    #[test]
    fn the_request_puts_the_listing_before_the_message() {
        let request = request(
            "openai/gpt-6-luna",
            "1. [decision] today — a: b\n",
            "fix the PPU",
            serde_json::json!({"reasoning": {"effort": "none"}}),
        );
        assert_eq!(request.system_prompt.as_deref(), Some(SELECTOR_PROMPT));
        assert!(request.tools.is_empty());
        assert_eq!(request.options["reasoning"]["effort"], "none");
        assert_eq!(
            request.messages,
            vec![ChatMessage {
                role: Role::User,
                content: vec![
                    ContentBlock::text("Notes:\n1. [decision] today — a: b\n"),
                    ContentBlock::text("Message:\nfix the PPU"),
                ],
            }]
        );
    }

    #[test]
    fn an_answer_picks_only_listed_numbers_once_and_at_most_max() {
        assert_eq!(picks(r#"{"picks": [2, 1]}"#, 3, 5), [1, 0]);
        assert_eq!(picks("```json\n{\"picks\": [3]}\n```", 3, 5), [2]);
        assert_eq!(picks(r#"{"picks": [0, 4, 2, 2, "1"]}"#, 3, 5), [1, 0]);
        assert_eq!(picks(r#"{"picks": [1, 2, 3, 4, 5, 6]}"#, 9, 5).len(), 5);
        assert!(picks(r#"{"picks": []}"#, 3, 5).is_empty());
        assert!(picks("I think note 2 fits", 3, 5).is_empty());
    }
}
