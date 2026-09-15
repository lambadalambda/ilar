# sudo asks for the password after the yes

## Summary

Seen 2026-09-15 on a real sudo: the first prompt said "type the
password" with no visible field, an empty answer was taken as "this
system needs none", and the second prompt had no field at all, so the
command could never run. The password question is tangled into the
approval question, and it is asked before anyone knows whether sudo
wants a password at all.

## Requirements

- The grant prompt for `root` is approval only: once, this session,
  always, deny. No password field on it, in the TUI or the chat.
- After a yes, and only then, the tool finds out whether a password is
  needed: a stored or held `SUDO_PASSWORD` is used as is; otherwise
  `sudo -n true` is probed, and a system that passes runs with `-n`
  and never asks.
- Only when the probe fails is the password asked for, in its own
  prompt: title "sudo password", the command shown, a masked field
  with a cursor, paste accepted, Enter submits, Esc cancels (the tool
  then fails with "no password given"). An empty answer is refused in
  place ("sudo needs a password on this system"), not taken as none.
- A password sudo refuses ("incorrect password attempt") is forgotten
  and asked for again, up to three times, without re-asking approval;
  the re-ask says the last one was refused. A refused *stored* password
  is said to be stored and the typed one is held for the session over
  it.
- A password sudo accepts is held for the session; a standing grant
  with a held or stored password runs with no prompt at all.
- Headless (`ilar exec`, a scheduled turn): a standing grant runs on
  what is known; no password known and the probe failing is a refusal
  that names `ilar secret set SUDO_PASSWORD`.
- The gateway answers the password prompt with `/password <pw>`,
  deletes the inbound message the way `/unlock` does, and refuses a
  password given with `/grant`.
- docs/secrets.md and docs/interface.md describe the new order.

## Notes

- Core: one prompt channel carrying an enum of the grant ask and the
  password ask, so the drivers keep one watch loop; `Approval` loses
  its password; `GrantPrompt` loses `password_wanted`.
- The fake sudo in crates/ilar/tests/tools.rs must answer `-n true`
  both ways and refuse a wrong password like the real one.

Size: M. Source: user report 2026-09-15, after the sweep.
