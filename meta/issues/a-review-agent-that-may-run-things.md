# A review agent that may run things

## Summary

`explore` is shell-less on purpose: a read-only agent holds a shared
read lease so several can run at once, and a shell under a shared lease
means parallel reviewers running parallel builds over one target
directory (decided in [read-only-says-what-it-takes-away], archived).
The cost is that a review which needs to *run* the tests is delegated
to `build` — a mutable agent whose prompt says "you may edit" — and told
not to edit. That works, but it is the wrong agent wearing a note.

The honest third agent is `review`: serialized per checkout like
`build` (it takes the write lease, because running things collides),
with a shell, and with the write tools removed (`write`, `edit`; `bash`
stays, so the guard rail is the prompt plus the lease, not a boundary —
the same caveat `read_only` already carries). Its description says
what it has: read, glob, grep, webfetch and bash; it can run tests,
builds and git; it does not edit; one at a time.

## Requirements

- A built-in `review` agent: `AgentWorkspaceMode::Mutable` (serialized),
  toolset = the builtins minus `write` and `edit`, background by default
  like `explore` since nothing waits on a review.
- Its description names the toolset and the serialization, in the same
  shape as `explore`'s; its prompt says it may run anything and edits
  nothing, and that a finding is reported, not fixed.
- The `task` tool description's routing advice names it: inspection →
  `explore` (parallel), review that must run tests → `review` (one at a
  time), changes → `build`.
- docs/agents-and-skills.md lists three built-ins.

## Acceptance Criteria

- A `review` child can run `cargo test` and cannot call `edit` or
  `write` (the registry test that pins `explore`'s toolset gets a
  sibling).
- Two `review` tasks on one checkout serialize; a `review` and an
  `explore` do not block each other.

## Notes

Whether `read_only` should ever grow a shell was the open question in
the archived issue; the answer was no, for the lease reason above, and
this agent is the other half of that answer.

Size: S-M. Source: decision on read-only-says-what-it-takes-away,
2026-09-18.
