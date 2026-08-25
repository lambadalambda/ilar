# One truncation helper

## Summary

UTF-8-boundary truncation is implemented four times with divergent
semantics (read.rs:151-160 plain, grep.rs:273-283 with `…`,
error_body.rs:217, bash.rs:110-114 inline loop), char-count
truncation three times (web.rs:636, store.rs:189-198, bash.rs:200),
and the char-boundary tail-cut idiom twice within transcript.rs
alone (`preview_tail` vs `append_thought_tail`). Each new copy is a
fresh chance to get the boundary loop wrong.

## Requirements

- One shared byte-bounded and one char-bounded truncation helper
  (with/without ellipsis) in the core crate; all sites adopt them.

## Acceptance Criteria

- Existing tests pass; grep for the `is_char_boundary` decrement
  loop finds one implementation.

## Milestone

12 — Health sweep

## Outcome

`ilar::text`: one private `floor_boundary` walk (the only
`is_char_boundary` loop left in the crate, verified by grep) behind
`truncate_bytes[_ellipsis]`, `truncate_chars[_ellipsis]`, and
`tail_bytes`/`tail_str`. Eight sites adopted, five local copies
deleted; the byte/char ellipsis semantics stay deliberately
distinct (documented in the module header). transcript.rs's two
tail-cuts genuinely matched and collapsed — their `.unwrap_or(0)`
fallback was proven dead. The TUI's display-width text.rs is a
different axis and untouched.
