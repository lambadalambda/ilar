# MiniMax thinks out loud

## Summary

`opencode-go/minimax-m3` streams its reasoning as a literal
`<think>…</think>` block at the head of the text content rather than
in a `reasoning`/`reasoning_content` delta (live smoke, 2026-09-03:
`text="<think>The user is asking me to…</think>\n\n"`). ilar shows it
as ordinary assistant text, so the transcript opens with the model's
notes to itself, and the fold that hides thinking never sees it.

## Requirements

- In the chat mapper, recognise a `<think>` block that opens at the
  start of the content stream and route it to `ThinkingDelta` /
  `ThinkingCompleted`, closing on `</think>`; text after it streams as
  text. Do not scan mid-content — only a leading block, so a model
  quoting the tag later is left alone.
- Applies to any chat-wire model that does this (it is a MiniMax and
  older-Qwen habit), not keyed to a provider.

## Acceptance Criteria

- A wire test: content deltas `"<think>plan"`, `"</think>\n\nhi"` map
  to `ThinkingDelta("plan")`, `ThinkingCompleted`, `TextDelta("hi")`,
  and a tag split across deltas still closes.
- A leading `<think>` never reaches the transcript as text.

## Notes

- Found while adding the rows in
  [Qwen and MiniMax answer on the chat wire](qwen-and-minimax-answer-on-the-chat-wire.md).
