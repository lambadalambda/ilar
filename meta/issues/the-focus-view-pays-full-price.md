# The focus view pays full price

## Summary

Three costs, one surface. Clicking a busy agent row runs
`open_agent_focus` (main.rs:2039-2075) which `store.load`s the child
*while it is still streaming* and rebuilds its whole view — including
its own children to depth 8, results kept at the 256 KiB restore cap —
inline on the UI task: a click can freeze the frame for the size of
the child's log. The seeded view is a full second copy of data the
root transcript already folded into `child_lines`, and it grows
unbounded while focused. And `FocusView::touch` (app.rs:139-142)
marks dirty-from-0, so every child event batch re-renders the entire
focused transcript — markdown, highlight, wrap — at up to 20 fps.

Fix shape: seed on `spawn_blocking` behind a loading placeholder; cap
the seed and the live tail; have `apply_child_loop_event` return the
touched index the way the root path does (app.rs:988-989).

Related: [[the-focus-view-earns-its-chrome]],
[[focus-seeds-the-step-in-flight]].

Size: M. Source: sweep 2026-08-31, responsiveness & memory.

## Outcome (2026-08-31)

The click opens a placeholder immediately; `seed_agent_focus` replays
on a blocking worker and `land_agent_focus` lands only on the focus
that asked (retarget, close, and error all drop it — an error closes
the placeholder and notices). Review caught the seam: a TurnDone
consumed while the seed replayed had already fired the row-closer, so
`replace_lines` re-closes when `running` is false. The per-event full
re-render is gone — `apply_child_loop_event` returns the lowest
changed line, `FocusView::touch_from` marks narrowly, pinned by a
thousand-line test. Not done: the seed/live tail memory cap — a
focused child's own timeline is bounded by its actual work and
released on close; cap it if a pathological child ever matters.
