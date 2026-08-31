# Obsolete replays stop working

## Summary

Retargeting agent focus replaces its `spawn_blocking` handle, which detaches the old full transcript replay; closing focus leaves the in-flight handle owned by the loop until it finishes. Session-search preview navigation drops the old receiver and never retains the worker handle. Rapid navigation can therefore queue many obsolete full-log parses whose results are discarded, saturating the blocking pool and delaying useful work. Landing guards already prevent stale results from changing the current view; the defect is the work that continues behind them.

## Requirements

- Give focus seeds and search previews explicit cancellable ownership.
- Stop obsolete work when focus closes, selection changes, or a newer generation starts.
- Make already-started blocking scans cooperatively cancellation-aware; do not rely only on `JoinHandle::abort`.
- Keep newest-result-wins guards at the landing boundary.

## Acceptance Criteria

- Tests prove retargeting focus and rapidly changing preview selection stop obsolete workers.
- At most one useful worker per surface remains active after a retarget.
- Stale results can never land on the new target.

## Notes

- Source: `crates/ilar-tui/src/main.rs:2585-2590`, `2739-2805`, `3049-3056`, `4183-4192`.
- Found by the current codebase review.
