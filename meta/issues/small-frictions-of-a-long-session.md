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
