# Pin user requests through compaction

## Summary

A mid-turn compaction can summarize away the request being served. Real
case, session `9860bd12`: the opening message — "Can we do the
necessary changes to firehose to support bundled payments?" plus the
two PR links that define what bundled payments *are* — sits at event 2.
The `RecentSteps` cut landed at 245, so those 167 characters went into
the summarizer along with 771k characters of transcript, and the 2386-
character summary that came back does not mention the request or either
link. 446k characters of tool output were kept verbatim; the ask was
not.

The run still finished, because the summary's "Remaining:" list and two
later user messages survived inside the recency window. It knew what to
do next; it had lost why.

User messages are the cheapest and most load-bearing tokens in a
transcript — all three in that session together are under 400
characters. Their survival should not depend on a summarizer's
judgement.

## Requirements

- The stored compaction summary carries the verbatim user messages from
  the region it summarized, ahead of the model's prose.
- Bounded: a total character budget for the pinned block, keeping the
  first message of the summarized region (the objective) and then the
  most recent ones, with a note when older ones are dropped.
- Applies to every cut policy; `RecentSteps` is where it is
  load-bearing, but a `TurnBoundary` compaction of a long
  multi-request session loses the same way.
- No transcript schema change: pinning happens in the summary text, so
  replay and the `<compaction-summary>` fold are untouched.

## Acceptance Criteria

- A test drives a compaction whose cut is past the opening request and
  asserts that request appears verbatim in the summary the session
  stores.
- A test pins the budget: many long requests are truncated to the cap,
  the first one survives, and the drop is stated.
- The full suite passes.

## Notes

- Pairs with [reframe the summarizer prompt](reframe-the-summarizer-prompt.md):
  this issue guarantees the objective survives; that one improves
  everything around it.

### Prior art

Codex does exactly this, structurally. `build_compacted_history` in
`codex-rs/core/src/compact.rs` builds the replacement history as
initial context + **the user messages, verbatim** + the summary, with
`COMPACT_USER_MESSAGE_MAX_TOKENS = 20_000`. It fills the budget
newest-first and truncates the message that straddles the boundary,
then restores chronological order; previous summaries are filtered out
so they cannot accumulate. Codex keeps *no* verbatim tool output after
compaction — user messages and the summary are the whole history.

Filling newest-first means a very long session can still evict the
oldest request. Keeping the first message plus newest-first for the
rest is strictly better for objective retention, at the cost of one
extra rule.

opencode does not pin messages; it relies on an `## Objective` section
in a structured template instead.

## Milestone

10 — Everyday polish
