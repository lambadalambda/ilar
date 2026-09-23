# The chat front door

## Summary

What a person meets first, and what they meet after a restart.

## Requirements

- Telegram's first message is `/start`, answered "No command /start."
  plus the whole help (commands.rs:147, gateway.rs:1090). Greet.
- Text that starts with `/` never reaches the model:
  "/etc/hosts is broken…" is `Unknown("etc/hosts")` plus help. Treat
  only `/[a-z]+(@thisbot)?` as a command; in groups ignore
  `/x@otherbot` (telegram/mod.rs:234, 469-481).
- An old grant button answers the ask open *now*: the keyboard stays on
  a timed-out ask and carries a bare `/grant always` (grants.rs:244-252,
  456, 461). Remove the keyboard when the ask closes, or carry an ask
  id and refuse a mismatch.
- A command in the startup backlog is skipped before the secret check
  (telegram/mod.rs:661-664): `/unlock <pw>` right after `/restart` is
  dropped and stays in the chat. Parse, delete if secret-bearing, and
  say "came in while I was restarting; nothing ran".
- `/status`, `/cost`, `/tasks` say "no session" after any restart
  (`seat_by_key`, gateway.rs:794/851/993, driver.rs:223); resolve
  through routes as `/model` does.
- An empty `allow_from`, a bad token or a missing rpc server does not
  stop the gateway (channel `run` bails, gateway.rs:427-438 restarts
  every 5 s forever). Check `allow_from` in `channels_from`
  (main.rs:115), as a missing token already is. Docs promise it.
- A new Telegram user cannot learn their numeric id (`/whoami` never
  answers strangers). Document the `@name` first, then `/whoami` path.
- `/status` shows no session id, so `ilar --view` of a chat needs
  routes.json. Show it.

## Acceptance Criteria

- Tests for /start, slash text to the model, stale grant button, the
  backlog unlock, /cost after restart, empty allow_from exits.
- Full gate green.

## Notes

- Source: UX sweep 2026-09-23 (gateway, docs passes). Size: M.
