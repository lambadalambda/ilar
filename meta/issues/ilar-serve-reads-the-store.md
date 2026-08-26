# ilar serve reads the store

## Summary

Phase 2 of the web frontend (see web-frontend.md; phase 1, `ilar
exec`, already exists). Decided direction (2026-08-26): a
**separate read-only process that tails the session store** — no
in-process coupling, no writer lease, no DTO layer, because the
append-only JSONL already is the wire format and its
forward-compat story is hardened. It supervises every ilar process
on the machine. Step-granular liveness (events land as steps
complete) is deliberate; token streaming is phase 3's in-process
concern. Frontend: minimal and dependency-light, no JS build step
in the repo — the server API is the durable artifact, the page is
replaceable; a framework may arrive later but must not take over
the repo.

## Requirements

- `ilar serve`: session list (the same summaries/ordering the
  picker shows, directory groups included), a session's transcript
  view, and a live tail per session over SSE.
- Tailing is correct under everything the store does: appends,
  rewind markers and folding, compaction, checkpoint republish,
  session create/delete mid-watch, torn tails (the incomplete last
  line rule).
- Binds 127.0.0.1 by default; any other bind requires a token
  (generated, shown once), and the docs state plainly what this is
  not (no sandbox, no authz model beyond the token).
- The page renders sessions usefully without a build step:
  transcript text, tool rows with results, image markers, usage —
  plain rendering first, polish later.

## Notes

- Design pass first (like the image feature): probe tail-following
  against live sessions, decide raw-events-vs-folded-view on the
  wire, the watch mechanism, and SSE fan-out shape; then slices.

## Milestone

11 — Beyond the terminal
