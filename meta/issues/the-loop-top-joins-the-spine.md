# The loop top joins the spine

## Summary

run_app's loop-top has become a second, untested spine: five
request flags smuggle dispatch→runtime messages around the Intent
system (two of them draining on a different clock than the other
two), the stall-watchdog episode and the search scan pump run
outside the schedule::Runtime seam, and the switch ritual is
duplicated six times with drift already visible (only one path
stops the scan; none cancels asides —
[[asides-do-not-outlive-the-switch]]). Fold flags into intents,
put the watchdog and scan pump behind the Runtime trait beside the
notification gate, extract one try_switch helper.

Size: M-L, incremental. Source: sweep 2026-08-29, event loop.
