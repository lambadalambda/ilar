# --view looks read-only

## Summary

`ilar --view <id>` (watch.rs:62-83, view.rs:998-1035) draws the full
input box titled ` input ` with the footer `Enter send ·
Shift-Enter/Ctrl-J newline · Ctrl-S stash`. Letters are dropped
silently; only Enter answers "read-only view: open the session
without --view to talk to it"; F1, Ctrl-F, Ctrl-L, Ctrl-T do
nothing. docs/sessions.md:13 says "typing into it is refused", which
only Enter does.

## Requirements

- Hide the input, or title it ` read-only · q leaves ` with no send
  footer, and answer the first keystroke rather than only Enter.

Size: S. Source: UX sweep 2026-09-15, TUI.
