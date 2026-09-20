# Help catches up with the surfaces

## Summary

The help overlay (modals.rs:793-882) predates milestone 14: no
agents panel, no focus view, no disclosures, no stall watchdog, and
Ctrl-Q's text omits deliveries. Two key inconsistencies ride along:
focus takes bare Home/End while the root transcript wants
Ctrl-Home/Ctrl-End, and Ctrl-P over a focus view is a timing race
between the peek and the poll (main.rs:1629-1654 vs 3505-3520) —
pick a policy and document it.

Size: S. Source: sweep 2026-08-29, event loop + rendering.

## Sweep 2026-09-03 additions

Still missing from the overlay (modals.rs:793-882): click an agents
row → focus view, Esc returns, ↑↓/PgUp/PgDn/Home/End and the wheel
scroll it (main.rs:3946-3961, app.rs:1673); click `N exited` / `+N
more`; Ctrl-N/Ctrl-P in every list (modals.rs:152-158); `d` in the
pending manager; and one line on what a `delivering` roster row and
a `✉ … delivered to …` line are — the thing the user found confusing
is explained nowhere in-app.

## Outcome (2026-09-20)

All of it, plus a measurement the issue did not ask for.

Added: a sidebar section naming the clickable rows and explaining a
`delivering` roster row and the `✉ … delivered to …` line that follows
it; the wheel and Home/End in a focus view; Ctrl-N and Ctrl-P, which
`nav_delta` has always taken in every picker; `d` in the pending
manager; and the stall watchdog's two thresholds. Ctrl-Q's row list
said "retry" and omitted services and the held results.

**The Ctrl-P policy needed no deciding.** The focus branch names the
root chords it forwards (`focus_key_belongs_to_the_root`) and sends the
rest to the prompt, so the overlay's existing sentence about the root's
chords is already true. The Home/End inconsistency is one-directional
and now said: bare keys work in focus, the root wants Ctrl.

**Six entries were being clipped and nobody had noticed**, including
the one this issue exists to add. `render_help` takes
`centered_rect(.., 72, ..)` in columns, not a percentage, so the action
column is 43 wide at every terminal size; the first version of the test
rendered at 80 and asserted on text the reader never sees. It renders
at the real width now.
