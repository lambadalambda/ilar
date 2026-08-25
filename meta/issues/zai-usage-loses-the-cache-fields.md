# zai usage loses the cache fields

## Summary

The z.ai Anthropic-flavor mapper reads only `input_tokens` from
`message_start` (zai.rs:544-548) and discards
`cache_read_input_tokens`/`cache_creation_input_tokens` — the fields
the Anthropic protocol reports there. `merge_usage` handles them
correctly but only runs for `message_delta`, whose usage typically
carries just `output_tokens`. Since the mapper marks usage
`ExcludesCached` and `context_tokens()` adds cache fields back in, a
warm cached turn undercounts context by the entire cached prefix: the
context meter and compaction trigger run far behind reality.

## Requirements

- Route `message_start.usage` through `merge_usage` so cache fields
  and accounting are captured.
- A stream whose `message_delta` carries no usage must still leave
  `input_token_accounting` set.

## Acceptance Criteria

- A mapper test: `message_start` with cache_read tokens yields a
  final usage whose `context_tokens()` includes the cached prefix.

## Milestone

12 — Health sweep
