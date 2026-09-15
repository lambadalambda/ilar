# A locked store is livable

## Summary

Once the master-password prompt is behind you, a locked store has no
way out and barely a trace:

- A mistyped password at startup exits: `store.unlock(&password)?`
  (secret_cli.rs:60) turns "wrong master password" into `Error: …`
  and exit 1 for the TUI and `ilar exec` (main.rs:1134, 1216). No
  second try, no fallback to running locked as Enter offers.
- `ilar exec` with no terminal (cron, systemd, CI) dies at the
  prompt: rpassword opens `/dev/tty` and fails with `Error: reading
  the master password: …` instead of running locked.
- Enter at the prompt prints "The secret store stays locked this
  session" to stderr (main.rs:118) a moment before the alternate
  screen takes over; nothing in the status or notice line says the
  store is locked afterwards.
- Every refusal then says "unlock it with the master password"
  (secrets.rs:67) and the `secrets` tool says "the user unlocks it
  with the master password" (secrets.rs:698), but neither the TUI
  nor exec has an unlock: only the gateway's `/unlock` exists. The
  gateway's own refusal never names `/unlock`.
- A provider key kept in the sealed store is silently skipped
  (`resolve_providers` swallows `Locked`, toml.rs:990); the "no
  provider configured for X (set ILAR_…_API_KEY …)" line
  (runtime.rs:399) and the gateway's startup log never mention the
  store as a place the key could be waiting.
- A store re-sealed under another password by a second process shows
  as "secrets: wrong master password" with no hint that anything
  changed, and `is_locked()` stays false so nothing re-asks
  (secrets.rs:264-274).

## Requirements

- Wrong password: re-ask (two or three tries), then continue locked.
- No tty: skip the prompt, run locked, say so once on stderr.
- A standing marker while locked (status line or a notice that
  stays), and refusals that say how to unlock on that driver:
  "restart ilar and enter it at the start prompt" for TUI/exec,
  "/unlock <master password>" for the gateway.
- The no-provider line and the gateway startup log mention a key
  that may sit in the locked store.
- A re-sealed store is reported as such.

Size: M. Source: UX sweep 2026-09-15, TUI + core + gateway.
