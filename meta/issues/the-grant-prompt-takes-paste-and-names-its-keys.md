# The grant prompt takes paste and names its keys

## Summary

The TUI grant modal (crates/ilar-tui/src/grants.rs) after the
password row landed:

- Paste is discarded: `Modal::Grant` maps to `PasteTarget::Discard`
  (decide.rs:121-133), so pasting a sudo password from a manager
  does nothing, silently. Route bracketed paste into the password
  row as the question modal does.
- With a password row the rows still read `(o) Allow once … (d)
  Deny` (grants.rs:141-148) while `o/s/a/d` type into the password
  (grants.rs:95-99); only the footer changes. Drop the hotkey
  prefixes when `password.is_some()`.
- The password row has no cursor and no width cap: one bullet per
  character in an unwrapped paragraph, clipped past ~60 columns with
  no sign (grants.rs:176-184). Show a trailing window or a fixed
  mask.
- A withdrawn prompt (the asker stopped listening, main.rs:3056-3062)
  vanishes without a transcript line and leaves `waiting for your
  grant` / `Activity::Paused` until the next event. Log "grant
  prompt for NAME withdrawn — the tool stopped waiting" and reset.
- The help overlay has no grant section, and its Input line "Esc /
  Ctrl-C — dismiss overlay · abort turn · clear input" is wrong under
  a grant, where both deny the tool and the turn goes on
  (main.rs:3454-3470, grants.rs:89). Add: o/s/a/d, Enter picks,
  Esc/Ctrl-C deny, letters type when a password row is shown.
- With several children running, "bash (subagent) wants NAME" does
  not say which one; `GrantPrompt` carries only `session_id`. Carry
  the child's name and show it.
- "Allow for this session" lands in the shared set (secrets.rs:656),
  so it covers the parent and every child; docs/secrets.md and
  docs/agents-and-skills.md do not say so, and the latter says
  nothing about children inheriting secrets or sudo at all.

Size: S-M. Source: UX sweep 2026-09-15, TUI.
