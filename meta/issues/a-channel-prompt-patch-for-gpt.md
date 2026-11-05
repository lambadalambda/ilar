# A channel prompt patch for GPT

## Summary

Codex's base instructions for GPT-5.6/6 and OpenCode's `gpt.txt` both
tell the model how its channels work: progress updates go to
commentary, the turn ends with a final message, and routine reads are
not narrated. OpenCode's Astra prompt adds "do not stop at proposing a
plan". ilar's base prompt says nothing like this, and long GPT runs
drift (session aed7fe15).

## Requirements

- A short prompt patch appended to the system prompt only for GPT rows
  (the ones flagged `message_phase` on the OpenAI family), not for
  Muse Spark, Grok or chat-wire models.
- Wording written for ilar, not copied: commentary for updates, final
  message ends the turn, no stopping at a plan or an offer to continue.

## Acceptance Criteria

- Unit test: the patch is in the system prompt for an `openai/gpt-6.1-sol`
  turn and absent for `opencode-go/muse-spark-1.3-contributor` and a
  GLM row.
- A live long-task run on the new build, compared against a run like
  aed7fe15, before closing.

## Notes

- Kept per model family so a patch for another family can follow the
  same shape.
