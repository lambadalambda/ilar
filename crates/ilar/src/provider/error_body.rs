use futures::StreamExt;

const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const TRUNCATED: &str = "...[truncated]";

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
        truncate_utf8(&mut sanitized, MAX_ERROR_BODY_BYTES - TRUNCATED.len());
        sanitized.push_str(TRUNCATED);
    } else {
        truncate_utf8(&mut sanitized, MAX_ERROR_BODY_BYTES);
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
                if sensitive_key(key) {
                    *value = serde_json::Value::String("<redacted>".into());
                } else {
                    redact_json(value);
                }
            }
        }
        serde_json::Value::Array(values) => values.iter_mut().for_each(redact_json),
        _ => {}
    }
}

fn redact_text(value: &str) -> String {
    let mut redact_next = false;
    value
        .split_whitespace()
        .map(|token| {
            if redact_next {
                let normalized = token.trim_matches(['\'', '"', ',', '{', '}']);
                if normalized.eq_ignore_ascii_case("bearer")
                    || normalized.eq_ignore_ascii_case("basic")
                    || normalized == "="
                    || normalized == ":"
                {
                    return token.to_string();
                }
                redact_next = false;
                return "<redacted>".to_string();
            }
            let normalized = token.trim_matches(['\'', '"', ',', '{', '}']);
            let lower = normalized.to_ascii_lowercase();
            if lower == "bearer" || lower == "basic" {
                redact_next = true;
                return token.to_string();
            }
            if normalized.starts_with("sk-")
                || normalized.starts_with("ghp_")
                || normalized.starts_with("github_pat_")
            {
                return "<redacted>".to_string();
            }
            if let Some((key, value)) = normalized.split_once(['=', ':'])
                && sensitive_key(key)
            {
                redact_next = value.is_empty();
                return format!("{key}=<redacted>");
            }
            if sensitive_key(normalized) {
                redact_next = true;
            }
            token.to_string()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn sensitive_key(key: &str) -> bool {
    let key = key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    [
        "token",
        "secret",
        "password",
        "authorization",
        "apikey",
        "credential",
        "cookie",
    ]
    .iter()
    .any(|needle| key.contains(needle))
}

fn truncate_utf8(value: &mut String, limit: usize) {
    if value.len() <= limit {
        return;
    }
    let mut boundary = limit;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
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
}
