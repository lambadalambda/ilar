# Scheduled turns report failure

## Summary

- `take_due` advances or retires a job before it runs
  (cron.rs:160-175); on `Err`, `handle_scheduled` only logs
  "scheduled turn failed" (gateway.rs:875), and "nothing to say" on
  zero sends. A "remind me at 15:00" one-shot that hits a provider
  5xx is retired and never fires, with no word in the chat. Post one
  line to the target ("Job <name> failed: …"); keep a failed
  one-shot for one retry.
- The cron tool wants UTC ("Five-field cron expression, in UTC",
  cron.rs:284-286) but nothing in the situation block or the system
  prompt states the current date, time or the person's timezone;
  under safe mode the model cannot run `date`. The only feedback is
  after a mistake ("at … is in the past; now is …"). Carry "now is
  <RFC 3339 with offset>" in the situation block, or accept a
  timezone and convert.

Size: S. Source: UX sweep 2026-09-15, gateway.
