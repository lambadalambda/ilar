# Images are not their base64

## Summary

Found 2026-08-28 in a session that compacted after its first
exchange: `estimate_tokens_from` counts an image block as
`image.data.len()` — the length of the *base64 string* — on the
comment "Base64 tokenizes roughly like text: count it". It does
not. A vision model bills an image by pixel area, tiled and
capped; the base64 is transport, not tokens.

The session attached six screenshots totalling 12.8 MB of base64.
The estimate read **3.2 M tokens**; the provider's reported usage
for that whole request was **24,994**. A 128× overestimate against
a trigger of 0.85 × 272k, so the second turn compacted a
conversation one exchange old — throwing away the images and the
task framing, which is exactly the context that mattered.

The estimate is `max(reported_usage, chars/4)`, so this poisons
every surface that reads it: the compaction trigger, the TUI
context meter, and `Compacted.context_tokens`.

## Requirements

- Estimate an image by what a vision model charges: pixel area over
  ~750, clamped to a sane floor and ceiling, with a flat fallback
  when dimensions cannot be read. `tools/binary.rs` already parses
  PNG dimensions; only the header needs decoding, not the payload.
- Same treatment for images carried on tool results, which have the
  same bug in the same function.
- A test with a realistic screenshot-sized image asserting the
  estimate stays within an order of magnitude of what providers
  report, and a regression test that a fresh session with attached
  images does not trip the threshold.

## Acceptance Criteria

- The afterglow session's six screenshots estimate to thousands of
  tokens, not millions; no compaction on a one-exchange session.

## Milestone

13 — Guard rails

## Outcome

`image::estimated_tokens` reads the dimensions out of the header
(only the first 64 base64 characters are decoded) and bills
width*height/750, floored at 200 and capped at 2400; an unreadable
header takes a flat 1600, because the payload size is exactly the
misleading quantity. Both call sites — attached images and
tool-result images — go through it. The afterglow session's six
screenshots drop from 3.2M estimated tokens to about 14k.

Two tests pin it, and one existing test had to be inverted: it
asserted `with_image - text_only == data.len() / 4` under a doc
comment that stated the false premise out loud ("Base64 tokenizes
roughly like text"). It now asserts that a payload a thousand times
larger costs the same, since neither declares readable dimensions.

Open question this raised, left for a decision: the estimate is
`max(reported, chars/4)`, which re-guesses the whole transcript
when the provider has already priced most of it exactly. Adding
the delta since the last reported usage to that exact number would
confine the guess to the untold tail — and would have prevented
this bug on its own, since the images were inside the request the
provider billed at 24,994.
