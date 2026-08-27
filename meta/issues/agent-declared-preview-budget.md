# Agent-declared preview budget

## Summary

Approved 2026-08-28: bash gains an optional `preview_bytes` — the
caller declares the output size it expects, and a surprise flood
costs that much preview instead of 30 KiB. Strictly better than
`| head` filtering-at-source: the full capture still spills, so a
wrong guess is a pointer away instead of a re-run.

## Requirements

- `preview_bytes` on the bash tool, clamped to 1 KiB..=30 KiB —
  lower-only, because a raisable cap invites "give me everything
  inline" and reintroduces the burn the cap exists to prevent.
- Failures ignore the declaration: non-zero exit, spawn error and
  timeout render with the full default budget, stderr share intact.
  The declared budget applies to the output the caller expected,
  not to the error it did not — a truncated diagnosis causes a
  re-run that costs more than the parameter ever saved.
- stderr keeps its half-share of whatever budget applies.
- Background bash honours it the same way.
- One-sentence schema description; docs note in the large-output
  section. Store-scan later tells whether it is used and whether
  re-runs follow it.

## Acceptance Criteria

- Success + small declaration + big output → preview near the
  declaration, note first, full capture in the spill file.
- Same command failing → full 30 KiB preview despite the
  declaration. Absurd values clamp instead of erroring.

## Milestone

13 — Guard rails
