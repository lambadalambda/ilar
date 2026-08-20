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

## Outcome

Seven move-only commits, one seam each, each independently green:
text, transcript, session_view, modals, input, selection + sidebar, app.
main.rs 13,362 -> 2,131 lines (startup and the event loop); largest
module app.rs at 5,078, two thirds of it tests. 172 tests throughout,
clippy clean.

`scripts/split_module.py` chunks Rust on column-0 item boundaries so
moved text is byte-identical, which made review "did the right blocks
move" rather than "was anything rewritten". Review verified that claim
independently on every seam.

`scripts/minimize_visibility.py` came later, after three consecutive
reviews found the `pub(crate)` regex over-exporting (24 of 51, 4 of 14,
46 of 113). It strips every marker and re-adds only what rustc's privacy
diagnostics demand.

Deferred to [TUI module follow-ups](tui-module-follow-ups.md) and
[Make the event loop testable](testable-event-loop.md).

## Notes

- Mechanical but repo-wide, so it should not be tangled with behaviour
  fixes — land the correctness issues first and rebase this on top.
- Best done as a sequence of move-only commits, one seam at a time, each
  independently reviewable and each green.

## Milestone

6 — Hardening
