# Small frictions of a long session

## Summary

The low pile from the responsiveness & memory sweep — each too small
to carry an issue alone, none load-bearing, all real. Omnibus by
design; tick items off here rather than splitting.

- Exited services keep up to 256 KiB of output each, forever
  (`ServiceManager.services`, service.rs:87) — trim to a small tail
  once the exit has been read.
- `held_notifications` is unbounded (main.rs:2442) — a few MB worst
  case and the outbox holds the durable copy; cap and spill.
- Clipboard-image paste decodes + downscales + PNG-encodes inline on
  the UI task (main.rs:3714, app.rs:1816-1842) — hundreds of ms for
  a Retina screenshot; image-file drop likewise (app.rs:1795-1813).
  spawn_blocking both.
- ~5 O(entries) bookkeeping scans per clean frame — resume scan,
  animation flags, `row_count`, `is_empty`, `visible_rows` skip walk
  (transcript.rs:297, 367, 409, 420, 476) — cache row counts /
  maintain an animated-index list.
- `transcript_cells` scrapes the whole visible buffer every frame to
  detect selection invalidation (view.rs:1069-1077) — gate on an
  active selection or mouse-down.
- Drag events are not coalesced (main.rs:3903) — per-event work is
  O(1), so only matters at extremes.
- Slash-completion inventory rebuilt per frame while a `/` draft is
  visible (view.rs:1014-1017, app.rs:554-577) — cache it.
- `status_line` reads `$HOME` every frame (view.rs:388).
- Drag-resize rebuilds the whole render cache per distinct width
  (transcript.rs:286-289) — a short debounce would smooth it.
- Link picker scans the whole transcript inline on open
  (app.rs:779-781) — one-time, only hurts on huge sessions.
- `outbox::retire` takes a blocking flock on the UI task
  (main.rs:1766) — fine locally, a hung network mount freezes the
  loop.
- `/command` subtask start may full-load the parent log just to read
  the effective model (subagent.rs:888 via main.rs:1892) — pass
  `app.current_model` in the request.

Size: S each. Source: sweep 2026-08-31, responsiveness & memory.

## Progress (2026-09-20)

Done:

- **Exited services keep a tail, not everything.** 64 KiB, enough that
  `logs` can still answer its own documented maximum for the service
  that just died. Both ways a service ends, because `stop` — the one
  the tool tells the model to use — set `exited` itself and skipped
  the trim, and the trim's guard was that transition, so no later
  refresh could run it either.
- **`status_line` read `$HOME` every frame.** Once now; it does not
  change under a running process.
- **The clipboard image no longer decodes on the render task.** The
  read needs the clipboard handle and stays; the downscale and the PNG
  encode — the hundreds of milliseconds — go to a worker, and the
  image joins the draft when it lands.
- **The slash-completion inventory stopped cloning.** It was rebuilt
  per frame while a `/` draft was visible, and two `String` clones per
  entry were nearly all of that; it borrows now. *Not* cached: the two
  fields it reads are public, a test can replace one with a list of
  the same length, and no cheap key can tell that apart.

Struck, with the reason:

- **`transcript_cells` scraping every frame.** The obvious gate — "only
  with a selection" — is wrong, and the tests say so. The invalidation
  check compares this frame's cells against the previous frame's, and
  a selection is made *between* frames; gating leaves the first such
  frame with nothing to compare against, so output that changed in
  that window would be copied from the new cells at the old
  coordinates. Fixing it properly means changing how a selection is
  invalidated, not where the scrape happens. The reasoning is now a
  comment at the scrape.
- **`outbox::retire` taking a blocking lock on the render task.**
  Moving it off means a tombstone that may not land before the process
  exits, trading a certain rare duplicate delivery for a rare freeze.

Still open: `held_notifications` unbounded and the drag-resize debounce
(both want a number), the five O(entries) scans per frame (which
belong with [[live-rows-rerender-every-frame]]), and the `/command`
subtask's full parent-log load.

## Progress (2026-09-20, later)

The `/command` subtask's parent-log load is struck, with the reasons
spelled out so it is not picked up again as a one-liner.

The issue's own suggestion — "pass `app.current_model` in the request"
— is wrong, not merely eager. `input.model` outranks an agent
definition's own `model`, and the TUI has not read the definitions at
the point where it builds the request, so filling the field would
silently override an agent pinned to a model. The comment at the
construction site now says so.

Making the lookup cheap instead is the honest fix and is not small.
`head()` gives the *opening* model, which a later `ModelChange` makes
wrong. The replay checkpoint does cache `effective_model`, but only
exists after a compaction. And a tail scan for the last `ModelChange`
is wrong in the presence of a rewind, which folds events away — the
real path handles that and a shortcut would have to as well. What is
left is an index that records the effective model per session, which
is a store change with its own issue's worth of care.

## Done (2026-09-21)

- **`held_notifications` is capped** — 256, four times the channel's
  capacity, since `0684432`; the overflow is back-pressure into a
  path that has a message. Ticked here late.
- **Drag-resize** got no debounce and needs no number: a drag arrives
  as a run of resize events, and the poll now keeps the last of a run
  and hands on whatever non-resize event follows it. Rebuilds happen
  as fast as frames complete, not as fast as the terminal reports
  widths. Pinned by
  `a_run_of_resizes_keeps_only_the_last_and_stashes_what_follows`.

Struck, with the reason: **the five O(entries) scans per clean
frame.** An entry is a transcript line or a tool group, so "entries"
is thousands at the very most, and each scan is a field read per
entry — well under a hundred microseconds a frame against a 50 ms
frame budget. The animation pass that used to dominate those frames
is fixed in [[live-rows-rerender-every-frame]]; what is left here is
not measurable. Caching the counts would add invariants to a cache
whose one invariant is already load-bearing.

Everything in this omnibus is now done or struck.
