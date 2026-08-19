# Bound and parallelize the glob walk

## Summary

`glob` walks the entire workspace on every call regardless of the
pattern, with all ignore filters disabled. On `~/repos/yodl` (186
worktrees, 407+ `node_modules`) that is **24,683,808 entries** — and
three concurrent `glob` calls each repeat the full walk independently.
Observed: three calls still "executing" after 1m24s while a `grep` in the
same turn finished.

The pattern that triggered it was
`worktrees/manteca-manual-withdrawal/*`, a directory containing **34
entries**. A 726,000× overscan.

Causes, in `crates/ilar/src/tools/glob.rs`:

1. **The walk always starts at `ctx.cwd`** (line 63). The pattern's
   literal prefix is ignored; matching happens after enumeration.
2. **Every ignore filter is off** (lines 64–69): `.hidden(false)`,
   `.ignore(false)`, `.git_ignore(false)`, `.git_global(false)`,
   `.git_exclude(false)`. So `node_modules`, per-worktree `.git`, and
   build caches are all descended.
3. **`MAX_MATCHES` short-circuits on 1000 *matches*, not entries
   scanned** (line 90), so a narrow pattern never trips it and pays for
   the whole tree. The more precise the query, the longer it runs.
4. **Single-threaded** `.build()`, plus `sort_by_file_name` forcing a
   collect-and-sort per directory.
5. No deadline or entry budget; only cooperative per-entry cancellation.

`grep` already gets this right — it scopes via a `path` argument
(`grep.rs:60`) and builds with `.hidden(true).git_ignore(true)`
(`grep.rs:86`). That is why it completed while glob did not.

Measured target on the same tree, same `ignore` crate, via ripgrep:

```
rg --files | wc -l   →  95,027 files in 1.03s (935% CPU)
```

Filtering alone is a 260× reduction (24.7M → ~95k); parallelism gets the
rest. ~1s for a full-workspace glob is achievable.

## Requirements

- Root the walk at the pattern's literal prefix: `worktrees/x/*` opens
  one directory, not the workspace. Matching stays relative to `ctx.cwd`
  so results are unchanged. The prefix must not escape the workspace.
- Respect ignore files and skip hidden entries by default, matching
  `grep` and ripgrep.
- Provide an opt-in escape hatch for globbing ignored/hidden files, since
  finding build output or `.env` is a legitimate use.
- Walk in parallel, bounded to a sensible thread count.
- Bound entries scanned, not just matches, so a pathological tree
  degrades to a clearly-labelled truncated result instead of an apparent
  hang. Truncation must be distinguishable from "no matches".
- Preserve deterministic sorted output and the existing 1000-match cap.

## Acceptance Criteria

- Unit tests for the literal-prefix split: `src/**/*.rs` → `src`,
  `worktrees/x/*` → `worktrees/x`, `*.txt` → root, `**/foo` → root,
  `src/a[0-9]/b` → `src`, and rejection of `../escape`.
- Test: a gitignored file is absent by default and present with the
  opt-in flag.
- Test: a hidden file is absent by default.
- Test: exceeding the entry budget yields a truncated result labelled as
  such, not an error and not silence.
- Existing `glob_matches_nested_patterns` and `glob_stops_at_its_match_cap`
  pass unchanged.
- Manual: a full-workspace glob on `~/repos/yodl` completes in seconds.

## Notes

- Enabling ignore filters is a deliberate behaviour change: globbing for
  gitignored files stops working by default. `grep` already made this
  trade. The opt-in flag is the escape hatch.
- Separately: the `glob` crate has **no brace expansion**, so the
  `**/{route,client}` pattern in the triggering turn matched nothing and
  reported it as a legitimate empty result. Models write brace patterns
  routinely (fd, bash, fzf all support them). Worth either supporting or
  rejecting loudly rather than silently returning "(no matches)".

## Milestone

6 — Hardening
