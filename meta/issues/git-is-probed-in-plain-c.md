# Git is probed in plain C

## Summary

The worktree validator distinguishes "no repository here" by
matching git's English stderr ("not a git repository",
tools/mod.rs:136), but `git_output` strips only `GIT_*` env vars —
a German locale says "kein Git-Repository" and every
repositoryless-session validation fails with the wrong refusal.

## Fix

Pin `LC_ALL=C` (and `LANG`) in `git_output`.

Size: S. Source: sweep 2026-08-29, store/workspace. Introduced by
the probe-fix hardening of worktrees-anchor-to-the-repo-beneath.

## Outcome

`git_command` now builds every probe — `GIT_*` stripped as before, plus
`LC_ALL=C` and `LANG=C` — and `git_output` uses it. `LANGUAGE` needs no
handling: gettext ignores it once the message locale is C.
`checkpoint::git`, the other wrapper, got the same pin so shadow-ref
failures read the same on every machine.
