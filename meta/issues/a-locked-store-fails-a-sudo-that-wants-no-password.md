# A locked store fails a sudo that wants no password

## Summary

`sudo` looks for a stored `SUDO_PASSWORD` before it does anything else
with the command (sudo.rs:188). On a sealed store nobody has opened,
that read comes back `Err(Locked)` and the tool returns right there:

```rust
Err(error) => return ToolOutput::error(format!("sudo: {}", secrets.store_error(error))),
```

Twenty lines below is the path that should have run. When no password
is known, sudo asks *the system* before asking anyone: `passwordless()`
probes for a NOPASSWD rule or a live timestamp, and where there is one
the command runs with no password at all (sudo.rs:230-236). A locked
store never reaches it. So on a box where `sudo` needs no password —
which is most boxes that run ilar unattended — a sealed store refuses
every `sudo` call, and the refusal names a master password that has
nothing to do with why the command could not run.

A locked store is not "the password is wrong". It is "there is no
stored password", which is exactly the case the probe exists for.

Since the master password is now asked for at the moment it is wanted
([the-master-password-is-asked-for-when-it-is-needed]), a terminal
session usually opens the store before this read and never sees it.
The two drivers that cannot ask — `ilar exec`, and the gateway before
somebody sends `/unlock` — see it every time, and so does anyone who
cancels the prompt.

## Requirements

- A locked store reads as *no stored password* on this path, not as a
  failure: sudo goes on to the passwordless probe.
- A command that then turns out to want a password says that, with the
  lock as the reason the stored one could not be read — the diagnosis
  belongs where the password is actually missed.
- A store that is damaged rather than locked still fails: an unreadable
  file is not the same as one nobody has opened.

## Notes

`held_or_stored` is the read; it can distinguish `Locked`/`Resealed`
from a real error already (`error.is::<Locked>()`), which is what
`store_error` uses to word the refusal.

Found by review of the lazy-unlock change, 2026-09-18.

Size: S. Source: review 2026-09-18.
