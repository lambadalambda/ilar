# grep has what the model reaches for

## Summary

Over 2026-09-05..07 the models ran `rg`/`grep` through bash about 230
times instead of the grep tool: with `-A`/`-B` context about 60 times,
`-i` about 27, and usually a trailing `head` to cap results. bash is a
barrier tool with mutating access, so each of those searches serialized
behind everything else and, in a leased child worktree, held the
parent's edits. The grep tool has none of context, case-insensitivity,
a file filter or a result cap.

## Requirements

- `context`: lines before and after each match, rendered rg-style
  (`path-N-text` for context, `path:N:text` for matches, `--` between
  non-adjacent groups), bounded.
- `ignore_case`.
- `glob`: only files matching it are searched; a pattern without `/`
  matches the file name at any depth, one with `/` matches the path
  relative to cwd; `{a,b}` alternation as in the glob tool.
- `limit`: a match cap below the tool's own.

## Acceptance Criteria

- A test per parameter; existing grep tests unchanged.
