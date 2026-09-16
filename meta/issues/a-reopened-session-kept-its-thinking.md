# A reopened session kept its thinking

## Summary

Raw thinking is persisted as `Diagnostic { kind: Local }` — never
replayed to a provider, but kept so a reader of the log can see what
the model was doing. The restore fold drops it
(`session_view.rs:594`), so a session shows `▸ Thinking:` rows while
it is live and none at all once it is rebuilt from the log.

`ilar --view` is *always* a rebuilt view: it tails the file and
re-folds on every change. So the main way to watch the assistant work
shows a model that answers without ever thinking — which is what it
looked like on the gateway, where the local model's `<think>` output
is the only reasoning there is. The log had 57 thinking blocks for 57
answers; the screen had none.

A provider-approved `ReasoningSummary` already restores as a
collapsed `Line_::Thought` two arms up. Raw thinking is the same
thing from a model that does not hand back a replayable item, and it
should read the same way.

## Requirements

- The restore fold renders `Diagnostic { Local }` as a collapsed
  `Line_::Thought`, expandable like any other, exactly as
  `ReasoningSummary { completed: true }` does.
- `ContentBlock::Thinking` restores the same way: sessions written
  before the diagnostic split carry it directly.
- Nothing changes about what leaves the process: thinking still never
  reaches a provider, a chat, or `ilar serve`'s wire.

## Acceptance Criteria

- A restored session containing a local diagnostic shows a thought
  row; a turn error still shows as a system line.
- Tests cover both, including the old `Thinking` block shape.

Size: S. Source: user question about gateway sessions, 2026-09-16.
