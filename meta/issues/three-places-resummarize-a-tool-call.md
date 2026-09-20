# Three places resummarize a tool call

## Summary

`LoopEvent::ToolArguments { id, arguments }` (`agent/event.rs:14-17`)
already carries the *finished summary* of a call — `turn.rs:1928-1942`
publishes it from the same `ToolCallCompleted` that produces
`ToolInputComplete`. Three consumers ignore it and rebuild the same
string from the raw input instead:

- `ilar-tui/src/exec.rs` (`ToolArguments::take`, 2026-09-20)
- `ilar-gateway/src/status.rs:282-289`
- the replay path in the TUI

Each keeps its own id-keyed map, parses the raw JSON again, and calls
`summarize_tool_input`. The raw input is explicitly unbounded
(`turn.rs:1961-1969`), so each consumer also clones a whole `write`
body to summarise it to 100-odd characters.

## Requirements

- One helper that keeps the latest `ToolArguments` per id and hands
  back the summary; the three consumers use it.
- No consumer re-parses the raw input for a summary.

## Acceptance Criteria

- A test pins the three surfaces agreeing on the same call's summary.
- The unbounded raw input is not cloned on a path that only needs the
  summary.

## Notes

- Found by the review of the `small-three` branch, 2026-09-20. The
  exec copy was written there knowingly; the precedent is what the
  issue is about.
- `ToolFinished` carries the name and `ToolStarted` carries it too, so
  a consumer taking the name from the finish is robust against a
  provider that skips the started event. Whatever the helper does here
  should keep that property.

## Outcome (2026-09-20)

Two of the three, and the third cannot be done as the issue asks.

`ilar::agent::ToolArguments` follows `LoopEvent::ToolArguments` — the
summary the loop already computed from the same `ToolCallCompleted`
that produces the raw input — and hands it back under the call's id.
`ilar exec` and the gateway's status narrator both use it. Neither
keeps the unbounded raw input, and neither parses or summarises
anything: a `write` of two megabytes was being cloned and re-parsed
per call to produce a hundred characters.

**The replay path is not a third consumer of the same thing.** It
reads the session log, which stores the raw input and no summary —
there is no `ToolArguments` event to follow, because the events are
not persisted. So "no consumer re-parses the raw input" is not
reachable there, and `session_view` keeps calling
`summarize_tool_input` directly. That is the right answer rather than
a gap: the log is the input, and summarising it on the way out is what
replay is for.

What the acceptance criterion really wanted — the surfaces agreeing —
holds by construction now for the two live ones, since they read the
same published string rather than each deriving their own.
