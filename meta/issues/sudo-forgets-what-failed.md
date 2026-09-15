# sudo forgets what failed

## Summary

Two password states the sudo tool handles wrong (sudo.rs:179-190):

- An empty answer at the prompt is held as `""` (secrets.rs:911-916),
  which counts as a known password (secrets.rs:811-814), so every
  later ask comes without a password row. When `sudo -n` then fails,
  the note says "the user types one into the prompt", but the next
  prompt has no row to type into. The `-n` failure should forget the
  held empty value so the next ask offers the row again.
- A wrong password from the store (`SUDO_PASSWORD` set via CLI) is
  not forgotten by `forget_held`, yet the note claims "it is
  forgotten, and the next ask takes a new one". The next call reuses
  the same stored value, again with no row. Say instead "update it
  with: ilar secret set SUDO_PASSWORD".

Smaller, same tool: the description "The user is shown the exact
command and asked before it runs" (sudo.rs:76-81) is false under a
standing grant with a known password and headless; add "unless the
user granted it". In the chat, `/grant sesion hunter2` on a password
ask silently becomes password "sesion hunter2" (commands.rs:82-88);
when the first word looks like a misspelt span, say so.

Size: S. Source: UX sweep 2026-09-15, core.
