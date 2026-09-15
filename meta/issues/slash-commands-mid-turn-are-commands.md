# Slash commands mid-turn are commands

## Summary

`decide::submit` (decide.rs:331-368) carves out `/btw`, `/context`
and the maintenance commands; every other `/name` becomes
`Intent::Steer(text)` and main.rs:505-532 sends the string raw.
`prepare_prompt`, which arms `/goal`, expands `/command` and routes
`/skill`, runs only for `StartTurn` (main.rs:672). Mid-turn `/goal
ship the parser` shows "steering · next step: /goal ship the parser",
the model receives that literal line, no goal is armed, no notice.
The rewind-fork work fixed this for `/rewind` and `/fork` only.

## Requirements

- Route any `/name` through `prepare_prompt` before steering, or
  refuse with "wait for the current operation before /name".
- A test per command class.

Size: S. Source: UX sweep 2026-09-15, session lifecycle.
