# The lock is a prompt, not a notice

## Summary

A sealed store nobody has opened puts a standing line on the notice
row for the whole session: "secret store sealed — the master password
is asked for when one is needed". It was added with the lazy unlock so
that the first password prompt would not come as a surprise.

It is a warning about something that is not wrong. The store being
sealed is its normal state; the prompt that opens it already names the
tool that is waiting and why. A line that sits at the bottom for every
session that never touches a secret is noise, and the machinery behind
it — a store handle kept on the app, a per-frame lookup to notice the
prompt opening it — exists only to serve the noise.

## Requirements

- No standing notice for a sealed store. The master password prompt is
  the only place the lock shows up in the TUI.
- The app keeps no store handle and no lock flag for the row's sake.
- docs/secrets.md stops saying the notice row says so.

## Acceptance Criteria

- A sealed, unopened store renders no notice line.
- The prompt still comes up at the first call that needs the store, as
  before.

## Notes

Asked for 2026-09-18: "a locked store should not constantly show up as
a warning on the bottom. it should just ask when it's going to be
unlocked, that's it."

Size: XS.
