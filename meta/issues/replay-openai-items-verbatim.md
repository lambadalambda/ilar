# Replay OpenAI items in the shape they arrived

## Summary

Prompt cache reads collapse to zero on the ChatGPT backend, worst right
after a step with several tool calls. Measured over 160 sessions with
`scripts/cache_report.py`:

| condition | requests reading nothing |
| --- | --- |
| z.ai | 0% of 614 |
| openai, overall | 17% of 3303 |
| after a step with 1–2 tool calls | 6% |
| after a step with 6+ tool calls | 52% |
| prompt grew <2k since the last request | 6% |
| prompt grew 10–30k | 46% |

It is not the backend, and it is not our prefix drifting. Codex CLI on
the same account and the same endpoint reads a cache on **738 of 738**
eligible requests, including every one of the 31 that grew 10–30k. And
a prefix that had mutated would still leave the unchanged head cached
and report a *partial* read; zero on a 100k prompt whose first thousands
of tokens are pinned means the server matched nothing at all.

The difference is item identity. Codex replays each item as the API
returned it — `codex-rs/protocol/src/models.rs` serializes `id` whenever
it is present. ilar dropped it: the OpenAI adapter kept only `call_id`
from `response.output_item.added` and discarded the item id, then
replayed calls anonymously and messages as bare `{role, content}` pairs
instead of typed `message` items. With `store: false` the server has to
rebuild the item graph from what we send, and reasoning items reference
the calls that followed them by id. The miss rate scaling with *calls
per step* rather than reasoning items per step is the fingerprint.

## Requirements

- The provider's item id survives from the stream, through the session,
  back onto the wire.
- Messages replay as typed `message` items with `input_text` /
  `output_text` parts, matching what the API returns.
- Sessions recorded before the id was captured still load and replay;
  they simply have no id to send.
- Nothing changes for providers with no item identity (z.ai), which
  already cache perfectly.

## Acceptance Criteria

- A test pins the replayed shape: `type: message` with the right part
  type per role, `function_call` carrying its `id`, and a legacy call
  with no id replaying without one.
- A test pins that completing a call keeps the item id its announcement
  carried — the completion rewrites the block, so it is the easy place
  to lose it.
- `function_call_output.output` stays a plain string, which is the
  canonical form for text results.
- The full suite passes.

## Outcome

Closed by the above. Whether it fixes the cache is a measurement, not a
claim: the baseline to beat is **40% misses on appends over 2k**, and
`scripts/cache_report.py --all` reads it straight off the sessions after
a day of use. If the number does not move, the next candidate is message
item ids, which Codex also sends and this change deliberately leaves out
— they would have meant a new field on `ChatMessage` and its event, for
the part of the shape the data does not implicate.

## Notes

- `prompt_cache_key` was already being sent as of the previous fix; it
  influences routing but cannot make a changed prefix match, and on its
  own it did not move the miss rate.
- Item id and call id are different things: `call_id` pairs a call with
  its result and must keep doing so; `id` names the call as an item.

## Milestone

7 — Unscheduled
