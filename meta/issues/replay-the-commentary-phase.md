# Replay the commentary phase

## Summary

On the Responses wire, GPT (5.3-codex and later) and Muse Spark 1.3
tag the short text before a tool call with `phase: "commentary"`.
ilar drops the field, so every replayed "I'll do X:" reaches the
model as a final answer. Muse Spark then copies the pattern: it
announces an action and ends the response without the call.

Replaying session be72f56b at its first stall: 5 of 6 samples stop
without a call today, 0 of 6 with `phase` restored on the earlier
lead-ins (all reasoning items kept, so caching is unaffected).

Grok sends its text after the call and no phase, so the tag must be
per model.

## Requirements

- A catalog flag marks the rows whose Responses wire uses `phase`.
- For those rows, assistant text in a message that also holds a tool
  call goes back with `phase: "commentary"` (derived; works for old
  sessions).
- `ContentBlock` gets a `text()` constructor so text literals and
  patterns stop spreading the field list (refactor, no behavior
  change).
- The stream's `phase` is stored on the text block and replayed as
  received; the derived tag is the fallback when none was stored.

## Acceptance Criteria

- Request-builder tests: a flagged row tags the lead-in, an unflagged
  row (Grok) does not, a final answer stays untagged.
- Parser test: a `message` item with `phase` lands on the stored text
  block, and replays unchanged.
- Old session files load unchanged.

## Notes

- Probed 2026-11-05 on Zen/Go: commentary from gpt-6-sol, 6-luna,
  6-astra, 5.6-terra, 5.6-luna, 5.5, 5.4-mini, 5.4-nano, 5.3-codex,
  muse-spark-1.3 (both). No phase from grok-4.7, grok-build-0.1.
  muse-spark-1.2 wrote no text in 8 tries, so it stays unflagged.
- Meta: dev.meta.ai/docs/protocols/responses ("preserve and resend
  it"); OpenAI: Codex prompting guide ("designed to prevent early
  stopping").
