# Ring terminal bell after turn

## Summary

Emit a terminal bell when a foreground turn releases control back to the user so
terminal emulators can visibly mark the tab as needing attention.

## Requirements

- Emit one ASCII bell after a foreground turn finishes cleanup.
- Cover successful, failed, and aborted foreground turns.
- Do not ring for intermediate events or background-agent notifications.
- Keep terminal teardown and event-loop error handling stable.

## Acceptance Criteria

- A terminal such as Ghostty receives one bell when input is needed after a turn.
- The bell is not repeated for the same turn.
- Focused and workspace checks pass.
