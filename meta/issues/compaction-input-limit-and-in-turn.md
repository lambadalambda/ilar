# Compact against the input limit, and inside the turn

## Summary

A gpt-5.3-codex-spark session (`4466f66d`) died with "Your input exceeds
the context window of this model" after zero compactions. Two independent
defects; either one alone is enough to lose a session.

**1. Compaction never runs mid-turn.** `run_turn` calls
`compact_if_needed_locked` once at `agent/turn.rs:886`, before the
`while iterations < config.max_iterations` loop at line 924. Context is
never re-checked across provider steps. The dead session had 3 user
messages and 44 assistant steps: input grew 1.5k → 127k inside a single
agentic turn, crossed the 108.8k trigger at step 32, and kept climbing
until the provider refused it. Compaction's next chance was a user
message that never arrived.

**2. The threshold is measured against total context; providers reject on
input alone.** `context_limit * threshold` uses the full window, but
spark is 128k total with a 100k input cap — so the trigger sits 8.8k
*above* what the API accepts. Even a perfectly timed check fires too
late. A catalog audit found this inversion in 26 of 45 models:

| model | context | input cap | fires at | over |
| --- | --- | --- | --- | --- |
| openai/gpt-5.3-codex-spark | 128k | 100k | 108.8k | +8.8k |
| openai/gpt-5.3-codex | 400k | 272k | 340k | +68k |
| zai/glm-4.7 | 205k | 74k | 174k | +100k |
| zai/glm-4.5 | 131k | 33k | 111k | +79k |

`ProviderResolver::input_limit()` (`provider/mod.rs:75`) already returns
the correct number. Nothing in production calls it — only tests.

The TUI context meter has the same bug: `main.rs:7189` reads
`resolver.context_limit()`, so spark displayed ~78% while already past
the hard cap.

## Requirements

- Compaction thresholds resolve against `input_limit`, falling back to
  `context_limit` when a provider exposes no input cap.
- Re-check the threshold between provider steps inside the turn loop, not
  only at turn start. Compacting mid-turn must preserve the in-flight
  tool-call/tool-result pairing so the next request stays well-formed.
- The TUI context meter and percentage read the same limit the
  compaction check uses, so the displayed headroom matches reality.
- Keep the existing force-compaction path working when no limit is known.

## Acceptance Criteria

- Mock test: a turn whose context crosses the threshold *between* steps
  (no new user message) compacts and completes instead of erroring.
- Mock test: a model with `input_limit < context_limit * threshold`
  compacts before exceeding `input_limit`.
- Catalog test asserting no model has `context_limit * threshold >
  input_limit` under the default threshold, so future entries can't
  reintroduce the inversion.
- Meter test: displayed percentage is computed from the same limit
  compaction uses.

## Notes

- Evidence: per-step input tokens from the dead session were
  `… 98211, 100410, 103977, 109020, 113659, 119914, 127194 ✗`.
- Mid-turn compaction is the load-bearing half. The threshold fix alone
  would not have saved this session; the in-turn check alone would have.
- Worth deciding whether a mid-turn compaction emits a visible
  `LoopEvent::Compacted` in the transcript — the user should see why the
  history collapsed under them.

## Milestone

6 — Hardening
