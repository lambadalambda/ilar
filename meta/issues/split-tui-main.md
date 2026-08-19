# Split the TUI main module

## Summary

`crates/ilar-tui/src/main.rs` is 12,983 lines: 347 functions, 123 tests,
and the entire frontend — transcript model, rendering, streaming,
markdown, input, six modals, session restore, and the event loop — in one
file alongside five small sibling modules (`diff`, `highlight`,
`history`, `markdown`, `theme`) that already show the intended shape.

`App` carries 66 fields mixing unrelated concerns: transcript model,
input buffer, search state, six modal handles, streaming statistics,
usage/cost accounting, clipboard and selection geometry, and service
snapshots. Nothing enforces which fields are allowed to interact, which
is how `has_modal()` came to omit `search_active` (see
[Unify the TUI modal layer](unify-tui-modals.md)).

`AGENTS.md` still states "The TUI crate has no tests by design", which
stopped being true at 123 tests.

## Requirements

- Split `main.rs` along the seams the file already has: transcript model
  and entry rendering, the render/layout pass, modals, session restore
  and notification display, the event loop, and formatting helpers.
- Group `App`'s fields into cohesive sub-structs (transcript/scroll,
  input/history, search, streaming and usage, selection/clipboard) so
  each rendering and dispatch site borrows only what it needs.
- Move each test next to the code it covers as the modules land.
- Correct the testing claim in `AGENTS.md`.

## Acceptance Criteria

- No behaviour change: the full test suite passes unchanged before and
  after, and `cargo clippy --workspace` stays clean.
- No module retains a majority of the original file's length.
- `AGENTS.md` describes the TUI crate's actual test story.

## Notes

- Mechanical but repo-wide, so it should not be tangled with behaviour
  fixes — land the correctness issues first and rebase this on top.
- Best done as a sequence of move-only commits, one seam at a time, each
  independently reviewable and each green.

## Milestone

6 — Hardening
