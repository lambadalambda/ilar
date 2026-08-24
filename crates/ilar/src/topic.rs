//! Naming a session in a few words.
//!
//! The first message is a poor title: often a stack trace, a link, or
//! "hey can you look at something". One short call after the first turn
//! buys a listing you can actually scan.
//!
//! Never on the critical path. Titling runs after a turn, and a failure
//! leaves the session titled the way it already was.

use anyhow::Result;
use futures::StreamExt;

use crate::provider::{Provider, ProviderEvent, Request, StopReason};
use crate::session::{ChatMessage, ContentBlock, Role, SessionEvent, SessionStore, new_id};

const INSTRUCTION: &str = "Name the topic of this conversation in three to six words, as a \
noun phrase a person would recognize in a list of sessions. Reply with the topic alone: no \
quotes, no punctuation at the end, no preamble, and do not answer or continue the \
conversation.";

/// Longest topic worth storing; anything past this is prose, not a name.
const MAX_TOPIC_CHARS: usize = 60;
/// Characters a model wraps titles in when it ignores the instruction.
const WRAPPERS: &[char] = &['"', '\'', '`', '“', '”', '‘', '’', '*', '#'];

/// Clean a model's answer into a topic, or reject it.
///
/// The model is being asked for a label and may answer with a sentence,
/// a quoted phrase, or a refusal. Anything that is not a short label is
/// better dropped than shown: the fallback is the opening message,
/// which is at least true.
pub fn clean_topic(raw: &str) -> Option<String> {
    let first = raw
        .trim()
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let trimmed = first
        .trim_matches(|character: char| WRAPPERS.contains(&character) || character.is_whitespace());
    let trimmed = trimmed.trim_end_matches(['.', '!', ':', ';']).trim();
    let collapsed = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    // A refusal or an answer is not a topic. Both start by talking
    // about the speaker.
    let opening = collapsed
        .chars()
        .take(20)
        .collect::<String>()
        .to_lowercase();
    for tell in [
        "i'm sorry",
        "i’m sorry",
        "i cannot",
        "i can't",
        "i can’t",
        "sure",
        "here is",
        "here's",
    ] {
        if opening.starts_with(tell) {
            return None;
        }
    }
    if collapsed.chars().count() > MAX_TOPIC_CHARS {
        return None;
    }
    Some(collapsed)
}

/// The one-message request: the conversation so far, then the ask.
/// Same shape as compaction, and for the same reason — a model shown a
/// conversation with the instruction in front of it answers the
/// conversation.
pub fn topic_request(
    model: &str,
    system_prompt: Option<&str>,
    transcript: &[ChatMessage],
    options: serde_json::Value,
) -> Request {
    let mut messages: Vec<ChatMessage> = transcript
        .iter()
        .map(|message| ChatMessage {
            role: message.role,
            // Tool traffic says how, not what; the topic comes from
            // what was asked and what was said back.
            content: message
                .content
                .iter()
                .filter(|block| matches!(block, ContentBlock::Text { .. }))
                .cloned()
                .collect(),
        })
        .filter(|message| !message.content.is_empty())
        .collect();
    messages.push(ChatMessage::user_text(INSTRUCTION));
    Request {
        model: model.to_string(),
        system_prompt: system_prompt.map(str::to_string),
        messages,
        tools: Vec::new(),
        continuations: Vec::new(),
        cache_key: None,
        options,
    }
}

/// Generate a topic for a session and record it. Returns the topic, or
/// `None` when there was nothing to name or the model gave nothing
/// usable.
pub async fn title_session(
    provider: &dyn Provider,
    store: &SessionStore,
    session_id: &str,
    system_prompt: Option<&str>,
) -> Result<Option<String>> {
    let reader = store.load(session_id)?;
    if reader.topic().is_some() {
        return Ok(None);
    }
    let transcript = reader.transcript();
    if !transcript.iter().any(|message| message.role == Role::User) {
        return Ok(None);
    }
    let model = reader.effective_model();
    let options = crate::model::variant_options(&model, reader.effective_variant().as_deref())?;
    let request = topic_request(&model, system_prompt, &transcript, options);
    drop(reader);

    let mut stream = provider.stream(request)?;
    let mut answer = String::new();
    while let Some(event) = stream.next().await {
        match event {
            ProviderEvent::TextDelta(text) => answer.push_str(&text),
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::EndTurn,
                ..
            } => break,
            ProviderEvent::TurnComplete { stop_reason, .. } => {
                anyhow::bail!("topic call ended with {stop_reason:?}")
            }
            ProviderEvent::Error(error) | ProviderEvent::RetryableError(error) => {
                anyhow::bail!("topic call failed: {error}")
            }
            _ => {}
        }
    }
    let Some(topic) = clean_topic(&answer) else {
        return Ok(None);
    };

    let mut session = store.acquire_writer(session_id)?.load()?;
    // Someone may have titled it while the call was in flight.
    if session.topic().is_some() {
        return Ok(None);
    }
    session.append(SessionEvent::Topic {
        id: new_id(),
        text: topic.clone(),
        ts: chrono::Utc::now(),
    })?;
    Ok(Some(topic))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_topic_is_a_label_or_it_is_nothing() {
        assert_eq!(
            clean_topic("Fixing the flaky auth test").as_deref(),
            Some("Fixing the flaky auth test")
        );
        assert_eq!(
            clean_topic("  \"Debugging a panic.\"  ").as_deref(),
            Some("Debugging a panic")
        );
        assert_eq!(
            clean_topic("**Bundled payments**").as_deref(),
            Some("Bundled payments")
        );
        assert_eq!(
            clean_topic("Session topic:\nRewriting the picker").as_deref(),
            Some("Session topic")
        );
        assert_eq!(
            clean_topic("firehose   bundled\tpayments").as_deref(),
            Some("firehose bundled payments")
        );

        // Not labels.
        assert_eq!(clean_topic(""), None);
        assert_eq!(clean_topic("   \n  "), None);
        assert_eq!(clean_topic("I'm sorry, I can't help with that"), None);
        assert_eq!(clean_topic("Sure! Here is the topic: auth"), None);
        assert_eq!(clean_topic(&"a very wordy answer ".repeat(10)), None);
    }

    #[test]
    fn the_request_asks_last_and_carries_only_what_was_said() {
        let transcript = vec![
            ChatMessage::user_text("fix the flaky auth test"),
            ChatMessage {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "found the race".into(),
                    },
                    ContentBlock::ToolCall {
                        id: "call-1".into(),
                        name: "bash".into(),
                        input: serde_json::json!({"command": "cargo test"}),
                        item_id: None,
                    },
                ],
            },
        ];

        let request = topic_request(
            "zai/glm-4.7",
            Some("system"),
            &transcript,
            serde_json::json!({}),
        );

        assert_eq!(request.messages.len(), 3);
        assert!(request.tools.is_empty());
        let rendered = format!("{:?}", request.messages);
        assert!(rendered.contains("fix the flaky auth test"), "{rendered}");
        assert!(rendered.contains("found the race"), "{rendered}");
        // Tool traffic is how, not what.
        assert!(!rendered.contains("cargo test"), "{rendered}");
        // The ask is the last thing the model reads.
        let last = format!("{:?}", request.messages.last().unwrap());
        assert!(last.contains("Name the topic"), "{last}");
    }
}
