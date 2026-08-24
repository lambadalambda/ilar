# Pin the Codex backend to a shard

## Summary

`prompt_cache_key` influences routing; it does not pin it. The Codex
backend takes the conversation's identity from headers — `session-id`
and `thread-id`, which `codex-rs/codex-api/src/requests/headers.rs`
builds on every request — and ilar sent neither. Without them each
request landed on whichever shard, and a shard either holds the prefix
or does not, which is why reads were binary: near-zero or the whole
previous prompt, never partial.

Measured live on `gpt-5.6-luna`, four arms alternating so the control
ran first both times (the second arm inherits a warmer backend):

| | follow-up steps that read a cache |
| --- | --- |
| without session headers | 2/10 |
| with session headers | **10/10** |

With the headers, every step read essentially the whole previous prompt
(10752 → 23040 → 35328 → 46592 → 58880 against prompts of 23k → 72k).

## Requirements

- The Codex backend receives `session-id` and `thread-id`, both the
  session's own id — the same value `prompt_cache_key` already carries.
- Only that backend: the public API has no use for them and a gateway
  may reject headers it does not know, so the rule matches
  `prompt_cache_key` — documented endpoint only.
- `cache_write_tokens` is parsed from the usage details, so a request
  that populates the cache can be told apart from one that finds
  nothing. Both are subsets of the input total, so carving them out must
  leave context and cost unchanged.

## Acceptance Criteria

- A test pins which providers get the headers and which do not.
- A test pins the usage split using the example from OpenAI's caching
  guide verbatim, including that `context_tokens()` is unchanged.
- The live probe remains runnable as the A/B that produced the numbers.
- The full suite passes.

## Outcome

Closed. The remaining measurement is the same one as before: the
baseline is 40% misses on appends over 2k, read back with
`scripts/cache_report.py --all` after real use. Field verification
came back: post-fix sessions read 0/9 and 1/19 misses against the 40%
baseline — archived on that evidence. Unlike the previous two
attempts this one has a live A/B behind it rather than a correlation.

Note that `cache_write_tokens` came back zero on every Codex-backend
request even when the cache was plainly being written — the field is
documented for GPT-5.6+ but this backend does not appear to report it.
The parsing is still right; it just stays zero here.

## Notes

- Found by reading the vendored Codex source rather than the docs: the
  docs describe `prompt_cache_key` as the routing lever and never
  mention these headers.

## Milestone

7 — Unscheduled
