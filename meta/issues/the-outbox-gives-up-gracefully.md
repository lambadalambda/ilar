# The outbox gives up gracefully

## Summary

A recovered completion that fails routing permanently — its agent
definition gone, its metadata unreadable — is announced at every
session open ("N task result(s) from a previous run will be
delivered"), attempted, failed into the transcript, and kept,
forever. The outbox has no notion of an entry that will never
deliver; the delivered-check only clears entries whose text
reached the log.

## Requirements

- An entry that fails delivery terminally (not Requeue-transient)
  is retired after its salvage into the transcript: the failed
  delivery already prints the child's final text, which IS the
  delivery of last resort — record it as such (append makes the
  delivered-check true, or mark the entry retired) so the next
  open does not repeat it.
- Transient failures keep retrying as today.
- A test: an entry for a session whose agent no longer exists is
  salvaged once and pends nothing afterwards.

## Milestone

13 — Guard rails

## Outcome

`outbox::retire` appends the notification as a tombstone to a
`.retired` sidecar (not `.jsonl`, so the pending scan cannot
mistake it for an outbox file and sweep it); append-only on both
sides, since a rewrite in retire could lose a concurrent record.
`pending()` treats retired texts as delivered, compacts them out,
and consumes the sidecar only after the rewrite provably
succeeded. The TUI calls it from the one place terminal failure is
known — the routed-completion Err arm, right after the salvage
into the transcript, which is the delivery of last resort. Requeue
outcomes never retire; the review traced what reaches Err (missing
metadata, unknown agent, registry failure — all pre-append,
nothing transient) and the classification stands as a decision:
an unknown agent retires because its salvage was shown.
