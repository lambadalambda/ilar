# Refusals that say the fix

## Summary

Refusals that leave the model guessing: `grep` and `glob` on a
missing directory return nothing; `webfetch` without a scheme
returns the URL parser's words; `read` on a directory returns the
OS error; `memory_get` drops unknown ids silently and the memory
cap says "consolidate" with no way to read the core; `skill_manage`
says "no skill" without the names, refuses a frontmatter patch
without pointing at rewrite, and replies "listed from the next
session on" to a delete; `task` resume with an unknown id gives a
store error; `service` start with an empty command says started;
`write` and `skill` have bare schemas.

## Requirements

- Each refusal above names what to do instead; `memory` gains a
  `show` action for the core files.

## Acceptance Criteria

- A test per message.
