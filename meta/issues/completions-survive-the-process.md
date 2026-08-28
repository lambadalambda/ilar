# Completions survive the process

## Summary

Found 2026-08-28 while auditing the routing pipeline. A background
child's completion exists in exactly one place while undelivered:
an in-memory mpsc channel (or the TUI's held queue). Quit drops it
without a word — the quit path warns about an invisible stash but
not about an undelivered task result. A session switch tears down
the runtime the same way. A crash loses everything in flight.

The child's session log has the work; the parent just never hears
about it. Nothing at session open looks for children that finished
after the parent stopped listening, so the result is permanently
stranded — the exact "we lost the subagent work again" experience,
even once in-process routing is airtight.

## Requirements

- Delivery state must be derivable from disk. A child session's
  meta records its parent; its log records its final turn. At
  session open (and after a turn, cheaply), find children whose
  completion never produced a `<task-notification>` user message in
  the parent, and requeue those as notifications.
- Quit warns when undelivered completions exist, like the stash
  warning: first press says what would be lost, second press quits
  anyway — and losing them must then be recoverable at next open by
  the scan above, so "lost" means "delayed".
- The scan must not misfire on tasks the parent already consumed
  (foreground tasks, delivered notifications, cancelled tasks the
  parent was told about).

## Acceptance Criteria

- Kill ilar with a finished-but-undelivered child; reopen the
  parent session; the completion arrives as a notification turn.

## Milestone

13 — Guard rails

## Outcome

Every published notification is appended to
`state_dir/outbox/<parent_session_id>.jsonl` before it enters the
channel — inside the permit guard's send, so even an
abnormal-death notification is durable. At session open the TUI
loads `outbox::pending`, which filters to entries whose parent
session still exists, whose ancestry reaches the opened root (so
concurrent ilar processes never adopt each other's trees), and
whose text does not yet appear in a UserMessage of the parent's
log — delivery is defined by the durable artifact itself, so no
delivery path anywhere needs to report back. Compaction rewrites
files as it reads them and sweeps dead sessions. Recovered
completions seed the held queue and announce themselves; the quit
warning now counts undelivered results and says they arrive next
open — they survive, so it warns of delay, not loss.

Known limitation, documented in code: two byte-identical
notification texts for one parent dedupe as one. Serve-side outbox
loading is noted in serve-kills-the-background-children's outcome.
