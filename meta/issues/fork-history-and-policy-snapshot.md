# Fork session history and permissions from one snapshot

## Summary

Forking a compacted session can copy conversation from one snapshot and recover inherited subagent model permissions from another if a concurrent rewind changes the source between reads.

## Requirements

- Build forked history and inherited permissions from one consistent source snapshot, or detect a changed source and refuse/retry.
- Preserve explicit deny-all permissions.
- Do not rely on caller-held writer ownership that the core fork APIs do not require.

## Acceptance criteria

- A deterministic regression test changes the source between the two reads and proves that mixed history and policy cannot be published.
- Ordinary fork, fork-at, compaction, and rewind tests still pass.

## Notes

- Deferred at the user's request. Do not implement yet.
- In `crates/ilar/src/session/store.rs`, `fork`/`fork_at` load a `SessionReader`; `fork_events` later calls `whole_events` to recover policy before `event_base`.
- Source reads do not acquire a writer lease. A rewind owns the writer lease, but that does not exclude these readers.
- An out-of-bounds prefix is rejected; replacement history of sufficient length can still pass.
- No frequency measurement exists. This is a core-API concurrency risk, not a confirmed normal UI reproduction.
