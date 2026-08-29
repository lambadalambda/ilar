# A web frontend

## Summary

Open-ended: we want something like
[openchamber](https://github.com/openchamber/openchamber) — a browser
workspace for running, supervising and reviewing agent work, reachable
from another device. Openchamber is a control plane *over* the opencode
CLI: a React/Vite PWA with desktop, VS Code and mobile shells, a
localhost-by-default server, terminals, an in-app preview browser, a
GitHub issue→PR flow, and multi-run (one task across up to five models,
diffed). This issue is the standing home for the question of how ilar
gets there, not a commitment to that feature list.

## What already fits

- `crates/ilar` has no TUI dependencies and `run_turn` is already a
  headless entry point taking an event channel.
- `LoopEvent` is close to a wire protocol: text and thinking deltas,
  the whole tool lifecycle, subagent configuration, provider retries,
  per-call usage.
- Sessions are append-only JSONL with replay, so history fetch and live
  tail are the same data; `SessionSummary`, `transcript_of`,
  `audit_events` and `children_of` are the read side.
- Human judgement points are already structured data, not terminal
  prompts: the `question` tool is a `QuestionRequest` over a channel.
- Supervision primitives exist: multi-session, fork at a point,
  checkpoints and rewind, worktree isolation, subagent activity and
  background notifications on their own channels.

## What does not fit yet

- `ilar-tui` is ~23k lines and a real share of it is app logic, not
  rendering: `decide.rs` (steer vs queue, maintenance carve-outs),
  `schedule.rs` (iteration ordering, already behind a `Runtime` trait),
  `session_view.rs` (events → view model), the `Line_` model inside
  `transcript.rs`. A second frontend either reimplements this and
  drifts, or it moves to a shared crate first.
- `LoopEvent` carries `std::time::Instant`; a serializable DTO layer
  has to sit between the loop and any socket, which is also where the
  protocol gets versioned.
- One process, one terminal, one writer. The session writer lease
  helps — a server holds it — but fan-out to several clients is new.
- No terminals, no preview, no GitHub flow. `service` runs long-lived
  processes but there is no PTY multiplexing.
- Safety: ilar has no sandbox and no permission system by design, and
  says so. Exposing it beyond localhost multiplies that surface. Any
  server ships with authentication, binds locally by default, and
  states plainly what it is not.

## Phasing

> Status 2026-08-28: phases 1-3 are done — exec, read-only serve,
> the workspace layout (serve-wears-the-workspace-layout) and the
> write path (serve-writes-back): send, steer, abort, new sessions.
> Open remainders here: a web answer modal for the question tool,
> model override on resume, and the far-field ideas below.

1. **`ilar exec`** — headless one-shot. Small, useful on its own, and
   it forces the core to prove it is drivable without a TUI, which is
   where the coupling shows.
2. **`ilar serve`, read-only** — a live event feed and session list in
   the browser. Most of the "supervise from elsewhere" value, no write
   path, no new policy.
3. **Interaction** — send, steer, answer a question, approve. This is
   the step that requires `decide.rs` and the view model to move into a
   shared crate; two implementations of "does this message steer or
   queue?" is a bug factory.

## Notes

- Alternative worth weighing before building any UI: make ilar speak a
  protocol an existing frontend already drives (opencode's server API,
  ACP). Far less work than a React app, at the cost of fitting ilar's
  model — checkpoints, rewind, resumable subagents — into someone
  else's abstractions.
- The openchamber description here comes from its README, not from
  reading its source.

## Milestone

11 — Beyond the terminal

## Parity gaps (sweep 2026-08-29)

- No todo panel: the web shows raw `todo` tool rows where the TUI
  renders the current list.
- The Subagents card is a flat list of direct children; the TUI
  panel is a tree — grandchildren are invisible without navigating
  into each child.
- A pending `question` in a TUI-driven session renders as an
  eternal spinner with nothing saying the session waits on a human
  elsewhere.

