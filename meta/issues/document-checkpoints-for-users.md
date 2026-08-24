# Document checkpoints for users

## Summary

The rewind feature's happy path is documented; the unhappy path is not.
The recovery recipe exists only as an implementation detail, the
submodule/sparse-checkout limitations live in a Rust doc comment, and
nothing tells users that the checkpoint chain is inspectable with plain
git or how to clean it up.

## Requirements

- A `docs/checkpoints.md` in the style of `docs/system-prompts.md`
  covering: how snapshots work and where they live; inspecting the
  chain with plain git (including cross-turn diffs); recovering from a
  rewind via the safety snapshot, with exact commands; limitations
  (ignored files invisible in both directions, submodules as gitlinks,
  sparse checkouts, conversation-only fallbacks); disk usage and
  cleanup.
- Every documented command is verified against a real checkpoint chain
  before being written down.
- The README's "Rewind and fork" section links to it and gains the one
  missing consequence sentence: ignored files are neither snapshotted
  nor restored.

## Acceptance Criteria

- The doc answers, without reading source: "I rewound and regret it —
  how do I get the old tree back?", "what did the agent change between
  turn 3 and now?", "why did my `.env` not come back?", and "how do I
  reclaim the disk space?".
- Commands shown were executed against an actual chain.

## Outcome

Written as `docs/checkpoints.md`; every command block was run against
the smoke-test repository's real three-commit chain (two turn
snapshots plus the rewind safety snapshot) before inclusion. README
links to the doc and states the ignored-files consequence.

## Milestone

8 — Time travel
