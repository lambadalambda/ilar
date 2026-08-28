# The TUI replays turn errors

## Summary

`serve` now shows why a turn died, because the projection learned
to pass `DiagnosticKind::TurnError`. The TUI still drops every
diagnostic on replay (`session_view.rs`), so resuming a session
that failed shows a transcript that simply stops — the same
blindness the web had, now the only one left. The live path is
fine; this is about what a resumed session shows.

## Requirements

- Replaying a `TurnError` diagnostic renders it as a system line in
  the transcript, in the error voice the TUI already uses.
- `Local` diagnostics (raw thinking, kept because no provider takes
  it back) stay dropped, as they are today.

## Acceptance Criteria

- A session whose turn failed shows the reason when resumed; a
  session carrying thinking-as-diagnostic renders no extra line.

## Milestone

13 — Guard rails
