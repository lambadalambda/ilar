# The assistant writes its skills

## Summary

The model can read skills through the `skill` tool and cannot write
them. Hermes's `skill_manage` (create, edit, patch, delete) is what
turns a workflow that worked into a skill for next time, with two
rules that keep the library small: "lessons, not logs" — a distilled
rule with its reason, never an incident narrative — and patch a
loaded skill first, then one in the same category, and create only
as a last resort.

## Requirements

- `skill_manage` for gateway sessions, writing under `<home>/skills/`,
  with the two rules in its description; the same `SKILL.md` format
  the `skill` tool reads.
- A usage ledger (`<home>/skills/.usage.json`): loads, views and
  patches per skill with timestamps, for the weekly review.
- Under the tool policy like any other tool; staged under
  `gateway.review.approval` like the review's writes.

## Acceptance Criteria

- Tests: a created skill is listed in the next session's prompt and
  loadable; a patch changes only what it names; the ledger counts.
