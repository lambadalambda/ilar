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
