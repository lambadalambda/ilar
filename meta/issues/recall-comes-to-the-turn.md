# Recall comes to the turn

## Summary

The archive is pull-only: nothing surfaces a note the model did not
think to search for, and models rarely reach for a recall tool on
their own (the harness study of 2026-09-17 measured it at well under
one call per task). Claude Code pushes instead: every user prompt, a
selector picks up to five memory files by their descriptions and
injects them as a hidden message attached to that turn. Its selector
is a sidecar model call; it also ships a model-free local index
behind a flag. ilar already has that index. This issue runs it on
every prompt and hands the model the hits.

## Requirements

- On every root-session user turn, run the memory index over the
  prompt text. Hits above a relevance floor, at most five, are
  appended after the user message as index lines, id, kind, age,
  title and summary, never bodies. The model reads a body with
  `memory_get` if it wants it.
- The block is framed as Claude Code frames it: retrieved for
  possible relevance, use only if it actually applies; a note older
  than a day carries its age with "verify before asserting".
- A note is not surfaced twice in one session, except after a
  compaction: the recalled set resets at the compaction cut, since
  the model lost the earlier copy.
- A per-session byte cap on injected recall, after which recall
  stops for the session.
- The injection is a session event of its own, adjacent to the user
  message like a checkpoint, so replay, `history`, the TUI and the
  gateway all see it the same way. It is never placed in the system
  prompt and never rewrites an earlier message.
- A frozen index at session open: the newest N notes' index lines
  injected beside the core block, once, so a fresh session knows what
  the archive holds. N small, capped in bytes.
- Off switches for both the per-turn recall and the session-open
  index, separate from the memory store itself.
- Both surfaces, TUI and gateway, get the same behaviour from the
  same core code. Room seats get none of it.

## Acceptance Criteria

- A wire test: a second turn whose prompt names a note's summary
  tokens sends that note's index line after the user message, and
  every earlier message is byte-identical to the first turn's.
- A test that a note recalled on turn two is not recalled on turn
  three, and is again after a compaction between them.
- A test that the session cap stops recall.
- A test that the session-open index appears once and does not
  change when a note is written mid-session.
- docs describe the recall block, the framing and the switches.

## Notes

Sub-issue 3 of 3, after [the-memory-store-moves-into-the-core] and
[the-tui-remembers-on-its-own]. The floor is the index's own score;
what the right floor is gets found live, on the gateway's archive,
before it is fixed. No sidecar model: a paid call per prompt for what
BM25 does is not worth it, and local models pay it in time.

Size: M. Source: comparison with Claude Code auto-memory, 2026-09-18.
