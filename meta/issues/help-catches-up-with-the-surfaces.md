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
