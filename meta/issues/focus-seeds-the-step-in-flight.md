# Focus seeds the step in flight

## Summary

Focusing a busy agent seeds from the store, and the store commits
step by step: whatever the child streamed since its last step
boundary — a running tool, half an assistant reply — is not in the
seed. The focus view now says so with a seam line, synthesizes a
row when a never-seen tool finishes, and follows everything from
the next event on. But the honest fix is to seed the in-flight
step itself.

## Requirements

- The seed includes the current step: either replay the session's
  `.live` scratch after the committed events, or splice the tail
  from the root transcript's nested `child_lines`, which have
  followed the broadcast since spawn (the splice point is the
  session's last committed step boundary).
- Deduplicate against the open-time race: an activity already
  broadcast but not yet drained must not fold twice on top of a
  seed that included it.
- The seam line goes away once the seed is whole.

## Acceptance Criteria

- Clicking an agent two minutes into a `cargo test` shows the
  running tool row immediately, with its elapsed time.
- No duplicated lines when focusing during a fast event burst.

## Notes

- Perf riders from the same review, worth taking together: the
  seed loads synchronously in the click handler (a huge child
  freezes the UI — the session-search preview solved this with an
  off-thread channel), and every focus event marks the render
  cache dirty from line 0 (fine for medium children, quadratic-ish
  for very long ones).
- Born from the adversarial review of
  [[a-clicked-agent-takes-the-screen]].

## Read-side design (sweep 2026-08-29, store territory)

The scratch is readable today — `live_path()` and
`parse_scratch()` are public, and the `TurnStarted{turn, step}`
generation header gives resync semantics — so this issue is not
blocked. What is missing is the read side as a component: a
`LiveTail` in `session/` owning both files' offsets, with the
ordering discipline written down (snapshot the committed offset,
read the scratch, re-check the committed offset — otherwise a step
commit between the reads splices step N+1 deltas onto a view
missing step N). Build it once there; serve wants it too.
Related: [[the-focus-view-settles-what-it-saw-running]] must land
with this or before it.

## Where it stands (2026-09-21)

Read for, not taken: the dedupe requirement has no mechanism behind
it, and choosing one is a wire decision.

The seed is committed events plus a seam line, built on a worker;
`replace_lines` lands it over whatever streamed in meanwhile. The
scratch would supply the in-flight step, and `LiveDelta` maps onto
`LoopEvent` closely enough to fold through `apply_child_loop_event`.
The trouble is the overlap. The scratch flushes on a deadline (~150
ms) or 4 KiB, so at the moment the seed reads it, it is a *prefix* of
what the broadcast has already delivered; the events folded since the
focus opened are a *suffix* of the same stream. The overlap between
them is exactly what would render twice, and nothing carried today
can locate it: `LoopEvent`s have no sequence number, and the scratch's
generation (`turn`, `step`) says which step, not how far into it.

Three ways out, each a decision rather than a fix:

- A per-step sequence number on `LoopEvent` and on each scratch line —
  the honest one, and a change to the core wire every surface reads.
- Fold the scratch and accept a bounded duplicate window (up to one
  flush interval of text) — visibly wrong sometimes, by design.
- Keep the seam and drop the requirement — what is on screen today,
  which at least never lies.

Also worth knowing before choosing: events that arrive between the
focus opening and the seed landing are discarded by the replace, not
just the ones before the open. The seam line covers that gap too.

