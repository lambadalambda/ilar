# Tools that destroy on empty input

## Summary

An audit after the message tool's fix found three tools that do
harm on input that cannot be meant: `edit` with an empty
`old_string` and `replace_all` inserts the new text between every
character; `skill_manage` patch without `new` deletes the passage,
and rewrite without `triggers` drops them; `memory` remove sweeps
every entry containing the text and says only "removed", and a
multi-line add silently becomes several entries.

## Requirements

- `edit` refuses an empty `old_string` and points at `write`.
- `skill_manage` patch requires `new` (an explicit empty string
  deletes); rewrite keeps the triggers when none are given.
- `memory` remove names what it removed and refuses when nothing
  matched; add refuses a multi-line text.

## Acceptance Criteria

- A test per refusal.
