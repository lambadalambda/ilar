//! Direct API smoke tests. Set ANTHROPIC_API_KEY, then run:
//! cargo test -p ilar --test smoke_anthropic -- --ignored

use futures::StreamExt;
use ilar::provider::anthropic::AnthropicProvider;
use ilar::provider::{Provider, ProviderEvent, Request, StopReason};
use ilar::session::{ChatMessage, InputTokenAccounting, Usage, new_id};
use serde_json::json;

const MODEL: &str = "anthropic/claude-haiku-5-5";

fn provider() -> AnthropicProvider {
    AnthropicProvider::new(
        std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY"),
        None,
    )
}

async fn reply(provider: &AnthropicProvider, system_prompt: Option<String>, prompt: &str) -> Usage {
    let events: Vec<_> = provider
        .stream(Request {
            system_prompt,
            messages: vec![ChatMessage::user_text(prompt)],
            options: json!({"max_tokens":16,"thinking":{"type":"disabled"}}),
            ..Request::with_model(MODEL)
        })
        .unwrap()
        .collect()
        .await;
    let Some(ProviderEvent::TurnComplete {
        stop_reason: StopReason::EndTurn | StopReason::MaxTokens,
        usage,
    }) = events.last()
    else {
        panic!("unexpected events: {events:?}");
    };
    assert!(
        !ProviderEvent::text_of(&events).trim().is_empty(),
        "{events:?}"
    );
    assert!(usage.output_tokens > 0);
    assert_eq!(
        usage.input_token_accounting,
        Some(InputTokenAccounting::ExcludesCached)
    );
    *usage
}

#[tokio::test]
#[ignore = "calls the paid Anthropic API"]
async fn live_short_turn() {
    reply(&provider(), None, "Reply with the single word Yes.").await;
}

#[tokio::test]
#[ignore = "calls the paid Anthropic API twice"]
async fn live_cache_write_then_read() {
    let provider = provider();
    // The random tag prevents a warm hit from a prior run. Keep the system
    // prefix byte-identical and comfortably above Haiku 5.5's 512-token minimum.
    let prefix = format!(
        "Test reference {}. Ignore the reference lines and answer the user's request.\n{}",
        new_id(),
        (0..128)
            .map(|i| format!("Reference row {i}: apples, oranges, bananas, pears.\n"))
            .collect::<String>()
    );
    let cold = reply(
        &provider,
        Some(prefix.clone()),
        "Reply with the single word Yes.",
    )
    .await;
    let warm = reply(&provider, Some(prefix), "Reply with the single word No.").await;
    println!("cold: {cold:?}; warm: {warm:?}");
    assert_eq!(cold.cache_read_input_tokens, 0, "cold usage: {cold:?}");
    assert!(
        cold.cache_creation_input_tokens >= 512,
        "cold usage: {cold:?}"
    );
    assert!(warm.cache_read_input_tokens >= 512, "warm usage: {warm:?}");
}
