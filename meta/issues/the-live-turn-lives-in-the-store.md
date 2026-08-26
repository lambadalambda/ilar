# The live turn lives in the store

## Summary

`ilar serve` shows liveness from mtime (wrong in both directions:
dead during long tool runs, live for 60 s after finishing) and
cannot stream tokens (deltas never touch disk). Approved direction
(2026-08-26): extend the store-is-the-wire principle — while a turn
runs, the turn loop writes batched deltas to an ephemeral
`sessions/<id>.live` sidecar; serve tails it. Streaming across the
net needs no microsecond latency: ~150 ms flushes against 250 ms
polls reads as live.

## Design

- **Scratch format** (`ilar::session::live`): JSONL lines of a
  small serde enum — `TurnStarted`, `TextDelta{text}`,
  `ThinkingDelta{text}`, `ToolStarted{id,name,summary}`,
  `ToolFinished{id,ok}` — deliberately tiny; anything else waits
  for the committed event. A `truncate` (step committed) resets the
  reader; file deletion = turn ended.
- **Writer** (core, so exec and subagents stream too): hooked where
  `run_turn` publishes loop events; buffered, flushed every ~150 ms
  or 4 KiB; created at turn start, truncated at each step commit,
  deleted at turn end (any outcome, aborts included — drop guard).
  Write failures are ignored (streaming is a luxury, the turn is
  not). Never fsynced.
- **Sweep**: leftovers from crashes deleted at startup alongside
  the spill sweep; serve treats a stale `.live` (mtime > ~60 s) as
  "stalled" — a supervision signal, not an error.
- **Liveness derived, not guessed**: working = `.live` fresh (with
  the current activity from its last lines, e.g. "bash: cargo
  test"); stalled = `.live` stale; idle = absent. The mtime window
  heuristic goes.
- **Serve**: the session tailer also tails the scratch (append
  between truncations, truncate = reset — the discipline it
  already has); SSE gains `delta` frames (no `id:` — ephemeral);
  the list rows show state + activity; the page renders a
  streaming tail row replaced when the committed event's `append`
  arrives.
- The `.live` file is NOT part of the audit log: never replayed,
  never listed, ignored by the store reader, `.gitignore`-class
  ephemera.

## Acceptance Criteria

- Writer: scratch created/truncated/deleted across a scripted turn
  (fake provider); deltas parse; abort deletes; a write failure
  doesn't fail the turn.
- Serve: delta frames stream between step commits and stop after
  the commit's `append`; liveness states (working/stalled/idle)
  pinned; stale scratch swept.
- End-to-end: a live `ilar exec` turn watched in a real browser
  shows text growing before the step commits.

## Milestone

11 — Beyond the terminal
