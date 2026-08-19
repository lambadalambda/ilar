# Goal mode

## Summary

Keep working across turns until a goal is achieved. Goals are often
nebulous ("recover engine function until 5 turns replay at 90%
accuracy"), so verification must be evidence-based, not a second-model
judge (which re-reads the whole context uncached every round).

## Requirements

- `/goal <description>` arms goal mode and submits the first working
  message; `/goal` alone shows/clears the current goal.
- After each completed turn, if the last assistant message does not
  contain a `GOAL_ACHIEVED` sentinel line, auto-submit a continuation
  prompt (same session — fully prompt-cached) that instructs the model
  to verify with concrete evidence (run tests/harnesses via tools;
  build a verifier if none exists) and keep working otherwise.
- Sentinel detection ends the loop with a notice; a round cap (default
  25) stops runaway loops; Esc on an idle blank input cancels goal mode.
- Queued user messages take precedence over goal continuations.
- Round counter visible (input title), rounds noted in the transcript.

## Acceptance Criteria

- Unit tests: sentinel detection, round accounting/cap, continuation
  precedence vs queue, /goal parsing.

## Notes

- Deliberately no TUI-side checker subprocess: the model runs checks
  itself with bash, keeping evidence visible in the transcript.
