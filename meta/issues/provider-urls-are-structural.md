# Provider URLs are structural

## Summary

Built-in provider `base_url` values are stored verbatim and endpoints are made by string concatenation. A trailing slash produces `//responses` or `//chat/completions`; malformed schemes, missing hosts, queries, and fragments are discovered only when a turn makes its first request. Custom-model URLs are better validated and trimmed, but should share one structural policy.

## Requirements

- Parse and normalize provider base URLs during configuration loading.
- Require an HTTP(S) URL with a host and reject query or fragment components.
- Join endpoint paths structurally and store one canonical base form.
- Reuse the same validation policy for custom models where applicable.

## Acceptance Criteria

- Tests cover trailing slashes, invalid schemes, missing hosts, query strings, and fragments.
- Valid configured provider and custom-model URLs produce exactly one path separator.
- Invalid values fail at startup with the originating config path and field.

## Notes

- Source: `crates/ilar/src/config/toml.rs:727-751`, `1119-1130`, `crates/ilar/src/provider/openai.rs:35-58`, `298`, and `crates/ilar/src/provider/chat.rs:197-205`.
- Found by the current codebase review.
