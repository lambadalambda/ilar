# The store changes under a running session

## Summary

What a running session shows once the store on disk no longer
matches what it was built from:

- The `secrets` tool is decided once at runtime build
  (`store.is_empty()`, runtime.rs:559-563) but child registries are
  built per spawn (subagent.rs:653-656). `ilar secret set X` on an
  empty store mid-session: the parent has no `secrets` tool while
  bash's schema says "(see the secrets tool)" (bash.rs:459), and any
  child spawned afterwards has it. Install it whenever a store file
  exists, or re-check per turn.
- The listing's root row is built only from `file.root`
  (secrets.rs:439-445, 710-729): a session-only root grant never
  shows, and the row shows in sessions with `agent.sudo = false`
  where no sudo tool exists. Synthesise from session grants too;
  drop it when sudo is off.
- An Always answer whose store write fails (re-sealed meanwhile)
  silently degrades to a session grant while the tool result and the
  transcript line say "(always)" (secrets.rs:925-934). Append "store
  unwritable; granted for this session only".
- `run_command` and the service log redact only the call's granted
  values (bash.rs:650, 664-665; service.rs:369-371); every other
  stored value reaches the spill file, the live tail and the service
  capture raw, and only the executor's final scrub hides it from the
  model. docs/secrets.md:43-49 claims source redaction for every
  stored value. Redact with `secrets.all()` at the source, or narrow
  the doc.

Size: S. Source: UX sweep 2026-09-15, core.
