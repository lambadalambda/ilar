# A restart finishes or explains

## Summary

docs/gateway.md:16-17 says Ctrl-C stops the gateway "waiting a few
seconds for turns in flight". The per-turn token is a child of the
gateway's cancel token (gateway.rs:138, driver.rs:439), so
`Gateway::cancel()` aborts every turn at once and `SHUTDOWN_GRACE`
only waits for the abort to land. The chat sees "⏹ ilar-gateway
stopping", "Aborted." (the `/abort` reply), then "▶ … started"; the
prompt in flight is never re-run (`run_leftovers` skips while
cancelled, gateway.rs:458-465). A `systemctl restart` after editing
SOUL.md, as the docs recommend, loses the question mid-flight.

## Requirements

- Either turns finish inside the grace, or the reply says "Aborted:
  the gateway is restarting; send that again." and the docs say so.

Size: S. Source: UX sweep 2026-09-15, gateway.
