# Agent endings use one vocabulary

## Summary

- One thing, six nouns: tool row `agent` (transcript.rs:2437);
  result row `task ▸` and "task result(s) held"; panel `agents (N)`;
  pending manager "background jobs" (app.rs:1705, which also counts
  deliveries); switch refusal "background agents"; foreground
  failures "subagent aborted" / "subagent failed" / "Subagent
  nesting limit reached". `agent` for the who, `task` for the what.
- Foreground and background endings differ for the same ending:
  "subagent aborted", "subagent failed: …", "(subagent finished with
  no text)" (subagent.rs:1396-1409) vs "Task "X" was aborted.",
  "Task "X" failed: …", "(finished with no text)" (1268-1296). One
  verb set through one headline rule.
- A user's cancel is reported as the grandchild's failure: a resume
  turn aborted by Ctrl-Q yields "Nested task "X" failed." / "Nested
  parent turn was cancelled." (subagent.rs:1947-1963); a delivery
  cancelled before its turn returns Requeue, whose Hold notice "…
  cannot reach it while it is busy — held" (schedule.rs:350-357)
  overwrites "background jobs cancelled" right after the user
  stopped it. Cancelled says cancelled.
- "task result held — send a message to retry" (schedule.rs:359) vs
  "… held — send a message to deliver" (view.rs:139).
- "undelivered result of X:" dumps the raw task-notification
  envelope as a System line (schedule.rs:365-397); every other
  surface collapses it. Route through `push_notification_row`.
- docs/interface.md:213, 224, 228 use `explorer`; the built-ins are
  `build` and `explore`.

Size: S. Source: UX sweep 2026-09-15, agents.
