use futures::StreamExt;

use crate::text::truncate_bytes;

const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const MAX_STREAM_ERROR_BYTES: usize = 4096;
const TRUNCATED: &str = "...[truncated]";

pub(super) fn stream_error_event(value: &serde_json::Value) -> super::ProviderEvent {
    let message = stream_error_message(value);
    if stream_error_is_retryable(value) {
        super::ProviderEvent::RetryableError(message)
    } else {
        super::ProviderEvent::Error(message)
    }
}

fn stream_error_is_retryable(value: &serde_json::Value) -> bool {
    [
        "/response/error/type",
        "/response/error/code",
        "/response/status_details/error/type",
        "/response/status_details/error/code",
        "/error/type",
        "/error/code",
        "/type",
        "/code",
    ]
    .into_iter()
    .filter_map(|pointer| value.pointer(pointer).and_then(serde_json::Value::as_str))
    .map(str::to_ascii_lowercase)
    .any(|kind| {
        [
            "overload",
            "rate_limit",
            "server_error",
            "timeout",
            "unavailable",
            "capacity",
        ]
        .iter()
        .any(|retryable| kind.contains(retryable))
    })
}

pub(super) fn stream_error_message(value: &serde_json::Value) -> String {
    for pointer in [
        "/response/error/message",
        "/response/status_details/error/message",
        "/error/message",
        "/message",
    ] {
        if let Some(message) = value
            .pointer(pointer)
            .and_then(serde_json::Value::as_str)
            .filter(|message| !message.is_empty())
        {
            return bounded_stream_text(redact_text(message));
        }
    }
    if let Some(error) = value
        .get("error")
        .and_then(serde_json::Value::as_str)
        .filter(|error| !error.is_empty())
    {
        return bounded_stream_text(redact_text(error));
    }

    let mut sanitized = value.clone();
    redact_json(&mut sanitized);
    bounded_stream_text(format!("provider stream error: {sanitized}"))
}

fn bounded_stream_text(mut value: String) -> String {
    if value.len() > MAX_STREAM_ERROR_BYTES {
        truncate_bytes(&mut value, MAX_STREAM_ERROR_BYTES - TRUNCATED.len());
        value.push_str(TRUNCATED);
    }
    value
}

pub(super) async fn bounded_error_body(response: reqwest::Response, secrets: &[&str]) -> String {
    let mut bytes = Vec::new();
    let mut truncated = false;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            truncated = true;
            break;
        };
        let remaining = MAX_ERROR_BODY_BYTES.saturating_sub(bytes.len());
        if chunk.len() > remaining {
            bytes.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            break;
        }
        bytes.extend_from_slice(&chunk);
        if bytes.len() == MAX_ERROR_BODY_BYTES {
            truncated = true;
            break;
        }
    }

    let raw = String::from_utf8_lossy(&bytes);
    let mut sanitized = serde_json::from_str::<serde_json::Value>(&raw)
        .map(|mut value| {
            redact_json(&mut value);
            value.to_string()
        })
        .unwrap_or_else(|_| redact_text(&raw));
    redact_explicit_secrets(&mut sanitized, secrets, truncated);
    if truncated {
        truncate_bytes(&mut sanitized, MAX_ERROR_BODY_BYTES - TRUNCATED.len());
        sanitized.push_str(TRUNCATED);
    } else {
        truncate_bytes(&mut sanitized, MAX_ERROR_BODY_BYTES);
    }
    sanitized
}

fn redact_explicit_secrets(sanitized: &mut String, secrets: &[&str], truncated: bool) {
    for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
        *sanitized = sanitized.replace(secret, "<redacted>");
        if truncated {
            let partial = secret
                .char_indices()
                .map(|(index, _)| index)
                .filter(|index| *index >= 4)
                .rev()
                .find(|index| sanitized.ends_with(&secret[..*index]));
            if let Some(length) = partial {
                sanitized.truncate(sanitized.len() - length);
                sanitized.push_str("<redacted>");
            }
        }
    }
}

fn redact_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(values) => {
            for (key, value) in values {
                if crate::redact::sensitive_key(key) {
                    *value = serde_json::Value::String("<redacted>".into());
                } else {
                    redact_json(value);
                }
            }
        }
        serde_json::Value::Array(values) => values.iter_mut().for_each(redact_json),
        serde_json::Value::String(value) => *value = redact_text(value),
        _ => {}
    }
}

/// A stranger's text, token by token. Shares every rule with the tool
/// row's own pass — one needle table, one set of shapes — and arms
/// more freely: see [`crate::redact::Mode`].
fn redact_text(value: &str) -> String {
    crate::redact::tokens(value, crate::redact::Mode::Untrusted, &mut Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_structured_and_plaintext_secrets() {
        let mut json = serde_json::json!({
            "error": {"api_key": "secret", "message": "safe"},
            "token_count": 2
        });
        redact_json(&mut json);
        assert_eq!(json["error"]["api_key"], "<redacted>");
        assert_eq!(json["token_count"], "<redacted>");

        let text = redact_text("Authorization: Bearer opaque --api-key=secret sk-live");
        assert!(!text.contains("opaque"), "{text}");
        assert!(!text.contains("secret"), "{text}");
        assert!(!text.contains("sk-live"), "{text}");

        let separated = redact_text("Password: hunter2 token = abc123");
        assert!(!separated.contains("hunter2"), "{separated}");
        assert!(!separated.contains("abc123"), "{separated}");

        let mut boundary = "request failed: super-sec".to_string();
        redact_explicit_secrets(&mut boundary, &["super-secret"], true);
        assert_eq!(boundary, "request failed: <redacted>");
    }

    /// The two holes the second copy had. Both were published — a
    /// provider that names a private key in its complaint, and one
    /// that quotes back the URL it was called with — and neither was
    /// visible from this file, which is the argument for there being
    /// one engine and not two.
    #[test]
    fn a_private_key_and_a_credentialed_url_survived_the_second_copy() {
        let mut json = serde_json::json!({
            "error": {"private_key": "-----BEGIN RSA-----", "endpoint": "safe"},
        });
        redact_json(&mut json);
        assert_eq!(json["error"]["private_key"], "<redacted>");

        let text = redact_text("could not reach https://bob:hunter2@api.example.com/v1");
        assert!(!text.contains("hunter2"), "{text}");
        assert!(text.contains("api.example.com"), "{text}");
    }

    #[test]
    fn structured_stream_codes_control_retryability() {
        assert!(matches!(
            stream_error_event(&serde_json::json!({
                "error": {"type": "rate_limit_error", "message": "slow down"}
            })),
            super::super::ProviderEvent::RetryableError(message) if message == "slow down"
        ));
        assert!(matches!(
            stream_error_event(&serde_json::json!({
                "error": {"code": "invalid_request_error", "message": "bad input"}
            })),
            super::super::ProviderEvent::Error(message) if message == "bad input"
        ));
    }

    #[test]
    fn stream_errors_use_common_messages_or_sanitized_fallbacks() {
        for value in [
            serde_json::json!({"response": {"error": {"message": "retry"}}}),
            serde_json::json!({"response": {"status_details": {"error": {"message": "retry"}}}}),
            serde_json::json!({"error": {"message": "retry"}}),
            serde_json::json!({"message": "retry"}),
        ] {
            assert_eq!(stream_error_message(&value), "retry");
        }
        let message = stream_error_message(&serde_json::json!({
            "error": {"message": "Authorization: Bearer sk-live"}
        }));
        assert!(!message.contains("sk-live"), "{message}");
        let fallback = stream_error_message(&serde_json::json!({
            "type": "error",
            "error": {
                "code": "failed",
                "api_key": "secret",
                "detail": "Authorization: Bearer sk-live"
            }
        }));
        assert!(fallback.contains("failed"), "{fallback}");
        assert!(!fallback.contains("secret"), "{fallback}");
        assert!(!fallback.contains("sk-live"), "{fallback}");
    }
}
