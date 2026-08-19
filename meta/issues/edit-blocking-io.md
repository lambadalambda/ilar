# edit blocks the async runtime

## Summary

`edit` is the only file tool that performs its I/O directly on the async
runtime. `edit.rs:58` calls `std::fs::read_to_string` inside the async
block, and `crate::atomic_file::replace` (line 79) writes the same way.
`read`, `glob`, and `grep` all go through `blocking_scan`; `write` uses
`run_blocking_io`. So a slow or large edit stalls a tokio worker thread
and, with it, whatever else the executor is running concurrently.

Two related gaps in the same function:

- **No size bound.** The whole file is read into a `String`, then
  `content.matches(&old).count()` scans it and `replace` allocates a
  second copy — roughly 3× the file size resident for a single edit.
  `read` caps its output at 256 KiB; `edit` has no cap at all.
- **`ctx.cancel` is ignored.** Every other file tool threads the
  cancellation flag through `blocking_scan` and checks it. Esc cannot
  interrupt an edit.

## Requirements

- Move the read, the replacement, and the atomic write onto the blocking
  pool via the same helper the sibling tools use.
- Observe `ctx.cancel`, so an in-flight edit aborts like every other file
  tool. Cancelling must not leave a partially written file — the atomic
  replace already gives all-or-nothing, so cancellation should be checked
  before the replace commits.
- Reject files above a size bound with a clear error rather than loading
  them, consistent with the caps `read` and `grep` already apply.

## Acceptance Criteria

- Test: editing a file larger than the bound errors without loading it,
  and the file is unchanged.
- Test: a cancelled edit returns the cancellation error and leaves the
  original content intact.
- Existing edit tests (unique match, ambiguous match, replace_all,
  not-found) pass unchanged.

## Notes

- Sizing the bound wants a moment's thought: source files are small, but
  legitimate edits to generated files, lockfiles, and fixtures can be a
  few MiB. `grep`'s per-file cap is 2 MiB, which is probably too tight
  here.

## Milestone

6 — Hardening
