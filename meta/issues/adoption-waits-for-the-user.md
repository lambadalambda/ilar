# Adoption waits for the user

## Summary

Opening a session is not inert. `outbox::pending` adopts every
completion the tree recorded but never delivered (main.rs:1284), and
the settle drain fires them on the first pass: a same-session parcel
starts a full model turn in the root (`start_notification_turn`), a
foreign one resumes its target session and runs a turn there
(`route_notification`, subagent.rs:1637) — immediately, even under a
modal, before the user touches a key. A turn is a real turn: token
spend, log mutation, tool execution in the workspace. Loading an old
transcript to *read* it can start *acting*, and a backlog stranded by
the pre-sweep delivery bugs means old sessions open onto a flood.

The design intent — quit mid-work, reopen, the gap's completions flow
in — is right. The trigger is wrong: process-open stands in for "the
user is back and engaged", which is false for browsing.

## Fix shape

Keep adoption (the outbox loses nothing by waiting). Open with
`notifications_paused = true` (main.rs:2447) and flip it on the
user's first submitted prompt, so same-session results steer into
that first turn — "delivered with your next message". The pause
plumbing exists (main.rs:1840-1845; the requeue backoff already uses
it). Reword the open notice from "will be delivered" to "held until
your next message".

## Acceptance Criteria

- Opening a session with pending outbox entries starts no turn and
  routes nothing until the user submits a prompt.
- After the first submitted prompt, the held results are delivered
  (steered into the running turn or as follow-up turns), exactly as
  they are today.
- The open notice says the results are held, not incoming.

Size: S. Source: user report 2026-08-31, old transcripts flooding
"task finished" on load.
