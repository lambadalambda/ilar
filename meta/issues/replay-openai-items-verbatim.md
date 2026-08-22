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

**The hypothesis did not survive its own test.** A live A/B on
`gpt-5.6-luna` (`live_chatgpt_item_id_cache_ab`) replayed one tool-heavy
conversation twice, identical but for the ids, four steps each with six
calls and ~75k appended per step:

| step | with ids | without ids (old) |
| --- | --- | --- |
| 2 | 0 | 10752 |
| 3 | 0 | 85504 |
| 4 | 161280 | 0 |

One of three follow-up steps cached with ids, two of three without.
Reads are binary — either near-zero or the whole previous prompt —
which is a shard that has the prefix or does not, not a prefix that
half-matches. Codex's own client confirms there is nothing else at the
request level to copy: `store: false`, `prompt_cache_key`, no retention
field, no `previous_response_id`.

Caveats, because the test is weak: three follow-up steps per arm is no
statistical power, the arms ran in a fixed order so the second arm saw a
warmer backend, and 75k per step is past the regime the sessions
actually live in. It shows no benefit; it does not prove none exists.

The change stays because it is what the API returned and what the
reference client replays, not because it was shown to fix anything.
Together with the August finding that *byte-identical* requests
alternated 0 / 6912 / 0, the evidence now points at backend shard
routing, with append size as a correlate rather than a cause.

## Notes

- `prompt_cache_key` was already being sent as of the previous fix; it
  influences routing but cannot make a changed prefix match, and on its
  own it did not move the miss rate.
- Item id and call id are different things: `call_id` pairs a call with
  its result and must keep doing so; `id` names the call as an item.

## Milestone

7 — Unscheduled
