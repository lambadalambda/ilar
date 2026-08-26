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

## Design (probed 2026-08-26)

Probe evidence that decided the shape:
- **P1**: macOS FSEvents does NOT surface appends through a held
  fd — ilar's writer holds one append fd for the session's life,
  so `notify` would show a frozen session then dump the turn at
  process exit. Polling is the design, not the fallback.
- **P2**: a 250 ms stat-poller caught every step at the right
  moment; a 1,090-entry directory scan costs 5 ms, a stat 2.7 µs.
- **P3**: `SessionStore::load()` fails 86.6% of calls under a live
  writer (stamp discipline protects the writer's checkpoint);
  snapshot-and-cut tail reads were 85/85 clean. A dedicated
  incremental reader is non-optional.
- **P4/P5**: the `.jsonl` is the only file a tailer needs — inode
  constant across rewind/compaction/repair; rewinds are appends;
  the committed line count is monotonic forever (only torn bytes
  are ever truncated). Sidecar `.replay.*` files are writer cache,
  never read by a tailer.
- **P6**: axum with default-features off adds 6 crates (hyper etc.
  already in the tree via reqwest). tiny_http would cost a thread
  per open tab. **P7/P8**: worst session 1.65 MB/906 events;
  `list()` is 178-577 ms (head-parses everything) — hence paging
  and an incremental head cache.

Decisions:
- **Watch**: hand-rolled polling (no notify dep) — 1 Hz directory
  scan driving the list and tailer lifecycle, 250 ms per-session
  tails only while subscribed; stat-derived resync triggers
  (shrink → repair resync; gone → deleted).
- **Wire**: canonical events in file order incl. rewind markers;
  the client fold is two lines (`rewind` → truncate, else push)
  because `Rewind.to` indexes the canonical stream. Payloads are
  projected server-side with the SAME core helpers the TUI uses:
  images → markers + lazy `GET .../images/...`, tool text →
  `bounded_detail` + full-text route, inputs → `summarize_*`.
  Children link (lazy `?invocation=` slice), never inline. SSE id =
  physical line number (monotonic) so `Last-Event-ID` reconnect is
  a re-read + skip. Compaction cut applied at render time only.
- **HTTP**: axum minimal features; GET-only router (the phase-2
  read-only boundary enforced structurally); assets `include_str!`
  (one binary, no path traversal).
- **Auth/bind**: 127.0.0.1:7777 default, no token (loopback can
  read the store anyway); any other bind generates a per-process
  256-bit token printed once as a URL (fragment-carried,
  sessionStorage, `?token=` for EventSource), constant-time
  compare, 401 no-body; plain HTTP — docs say VPN/tunnel it, never
  the public internet. `ILAR_SERVE_TOKEN` to pin one.
- **Page**: 3 static files, ~450 lines hand-written JS, hash
  routing, EventSource, plain-text-first rendering (fences, inline
  code, bare links, diff +/- classes; no markdown lib, no ANSI);
  `textContent` everywhere — never innerHTML with server data.
  >700 lines of JS is the agreed signal to revisit frameworks.
  List groups by cwd (no privileged "here" — the server has none).
- **Resolved open questions**: lands in `ilar-tui/src/serve/`
  (crate rename filed separately); `serve` skips provider
  validation entirely (needs only state_dir — starts with no API
  key configured); `--open` included (reuses `open_in_browser`).

Slices (1→2∥3→4→5→6): 1 core `session/tail.rs` incremental reader
(+ public `head()`, tested against store fixtures, zero HTTP);
2 projection `serve/view.rs`; 3 supervisor `serve/watch.rs`
(broadcast fan-out, subscriber lifecycle); 4 axum routes + SSE +
auth (integration-tested over a tempdir store on port 0); 5 the
page; 6 docs (`docs/serve.md`, security paragraph, DEVLOG-worthy
P1 finding recorded in docs/sessions.md instead — this repo's
record is the tracker).

## Milestone

11 — Beyond the terminal

## Outcome

Shipped across six slices, each reviewed and committed separately
(cd903a8 tail reader, 9c12639 projection, 32d8dd2 supervisor+API,
this commit the page+docs). The probes drove everything: polling
because FSEvents is blind to held-fd appends, a dedicated
incremental reader because load() fails 86.6% under a live writer,
snapshot handoff, line-numbered SSE resume. The page is 649 lines
of framework-free JS (tripwire 700), textContent-only, verified in
a real browser: live appends, rewind folding with a divider,
compaction cut, image thumbnails, nested child transcripts, and
all four token paths. Design deviations, all reviewed: `Rewound`
carries the marker event (the wire needs its id/ts); assets are
public under token auth (the fragment cannot ride the first
request — data routes stay gated); the list has title only (topic
folds into it). Recorded residuals: the list view rebuilds DOM
every 3s (keyed patching if keyboard nav arrives); watcher could
reuse an exposed read_head; LIVE_WINDOW untestable-false;
notification-routed child sessions tail fine (they are ordinary
files). Phase 3 — interaction — remains with the parent issue.
