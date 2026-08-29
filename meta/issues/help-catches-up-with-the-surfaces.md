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
