# Bring grep to parity with glob

## Summary

`grep` returns silently wrong results and can walk the whole filesystem.
Probe over a tree with `NEEDLE` in four files:

```
grep found:  ignored/x.rs   ← gitignored, should have been skipped
             src/a.rs
grep missed: .github/workflows/ci.yml    .env
```

Wrong in both directions, with no truncation marker or warning:

- `.hidden(true)` (`grep.rs:87`) skips every dotfile path — `.github/`,
  `.env`, `.claude/`, `.cargo/`. That is where CI config and env files
  live.
- The docstring (`grep.rs:33`) promises "Gitignored files are skipped",
  but `require_git` defaults to true, so outside a git repository
  `.gitignore` is not consulted at all.

Since [Bound and parallelize the glob walk](glob-walk-scoping-and-speed.md)
landed, `glob` behaves the *opposite* way on both axes — it keeps dotted
paths and honours ignore files via `require_git(false)`. Two sibling
tools now disagree about which files exist. glob's behaviour is the
correct one; grep should move to match it.

Two more, same class as the glob defect:

- **`path` is unvalidated.** `ctx.cwd.join(input.path)` (`grep.rs:60`)
  and `Path::join` on an absolute path replaces the base, so
  `{"path": "/"}` walks the entire disk. Verified:
  `{"pattern": "localhost", "path": "/etc"}` returns `/etc/hosts:4:…`.
  The parameter is documented as "Subdirectory to search", so escaping
  it contradicts its own contract.
- **The caps do not bind the expensive case.** `MAX_MATCHES = 200` only
  short-circuits when there *are* matches; a rare or no-match search
  pays for the entire tree. Single-threaded `.build()` makes it worse:

```
ilar grep, no match, ~/repos/yodl:   20.10s
rg, same search:                      4.10s  (1000% CPU)
```

## Requirements

- Keep hidden entries visible so `.github/**` and `.env` are searchable;
  drop `.git` explicitly, as glob does.
- Honour ignore files outside a git repository (`require_git(false)`), so
  the documented behaviour matches the actual behaviour.
- Offer the same `include_ignored` escape hatch glob has, with the same
  default.
- Validate `path`: reject absolute paths and `..` rather than walking
  outside the directory the tool was pointed at. Share the check with
  glob so the two cannot drift again.
- Walk in parallel and bound entries scanned, so a no-match search over a
  monorepo is seconds rather than tens of seconds and a pathological tree
  truncates with a distinct message.
- Output stays deterministic: sort by path then line number, since
  parallel walking loses walk order.
- Correct the docstring.

## Acceptance Criteria

- Test: a needle in `.github/workflows/ci.yml` and in `.env` is found by
  default.
- Test: a needle in a gitignored path is skipped by default, and found
  with `include_ignored`.
- Test: `path` values `..`, `/etc`, and `a/../..` are rejected with an
  error naming the workspace.
- Test: exceeding the entry budget truncates with a message distinct from
  the match cap.
- Test: results are ordered by path then line, independent of thread
  scheduling.
- glob and grep agree on which files are visible for the same tree.

## Notes

- Making grep skip gitignored files by default is a behaviour change in
  the opposite direction from the dotfile fix: `ignored/x.rs` currently
  gets found and will stop being found. `include_ignored` covers it.
- `read`, `write`, and `edit` share the unvalidated `cwd.join(path)`
  shape. Left alone deliberately: the README states ilar provides no
  sandbox and that worktree isolation is not a security boundary, so
  out-of-repo access there is documented design, not a defect. The
  reliability argument (a hallucinated absolute path silently succeeding
  instead of erroring) is real but is a separate decision.

## Milestone

6 — Hardening
