# One status line per seat

## Summary

- `begin_status` inserts a new `Status` under the seat key, silently
  replacing any existing one (gateway.rs:1082-1085); the replaced
  one is dropped without aborting its updater or deleting its
  message. A subagent finishing while the user's next turn runs
  (`handle_follow_up` calls `begin_status` before `run`,
  gateway.rs:734-735) leaves the older "running bash: …" bubble in
  the chat permanently and the follow-up with no status at all.
  `send` clears the status for any outbound to the chat
  (gateway.rs:1109-1111), so a cron job posting mid-turn takes the
  live turn's line down. Reuse or refuse when a status is up; only
  the owning turn clears it.
- A message sent while the bot works is steered with the only signal
  being "steered: …" on the status line (status.rs:262), which does
  not exist under `gateway.status = false` and is gone once the
  model has sent anything this turn. The person cannot tell a
  folded-in correction from a dropped message. A small ack, or the
  status line re-posted when a steer arrives with no line up.

Size: S. Source: UX sweep 2026-09-15, gateway.
