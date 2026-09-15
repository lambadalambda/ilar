# Configuration: small frictions

## Summary

Omnibus; tick items off here.

- `ilar exec` never reads `config.warnings` and never calls
  `startup_notices` (main.rs:995-1087, 1119-1127): a project
  `ilar.toml` with `[providers]` or `general.theme` is ignored in
  silence, against docs/configuration.md:8-9 and 314-315. Write them
  to stderr (and a JSON event under `--json`).
- `--agent` on a resumed session is honoured but never recorded
  (runtime.rs:115-117, 404-413 persists only a model override): the
  next `--continue` reverts silently. Persist it or say so.
- An unwritable state dir fails as "creating session / Permission
  denied" (runtime.rs:213) without naming the directory or
  `ILAR_STATE_DIR`.
- `ilar secret …` and `ilar login` resolve the full config first
  (main.rs:1132), including endpoint discovery at 3 s per endpoint
  (toml.rs:738-743), so a broken `ilar.toml` blocks fixing a key
  through the store. State-dir-only subcommands should resolve only
  the state dir.
- `HOME` unset falls back to `.` silently (toml.rs:582); sessions
  land in the project. Refuse or warn once.
- ilar.toml.example:27 says a project override can switch back with
  `auth = "api_key"`; project `[providers]` is discarded wholesale
  (toml.rs:671-676). docs/configuration.md:6-8 names four
  user-scoped tables; the code also pins `[endpoints]`,
  `[cache_compact]`, `[gateway]`, `[channels]`.
- README.md:131-133 says the workspace is two crates; it is three.

Size: S. Source: UX sweep 2026-09-15, first run.
