# Retry-now strands the pending manager

## Summary

The pending manager's "retry now" closes the modal only when the
decided intents contain `Intent::StartTurn` (main.rs:2231) — but
`decide::retry` (decide.rs:312-320) only ever returns
`Intent::ResumeTurn` or a notice. The close branch is unreachable:
the turn resumes underneath the still-open modal, which keeps the
keyboard and blocks notification routing until the user hits Esc.

## Requirements

- Close the pending manager when retry actually produces a
  `ResumeTurn`; leave it open on the draft-warning notice.

## Acceptance Criteria

- A test: retry-now with a blank input closes the modal and resumes;
  with an unsent draft the modal stays and the warning shows.

## Milestone

12 — Health sweep

## Outcome

The dismissal rule moved into the decision layer as
`decide::retry_dismisses_manager` (true iff the intents contain
`ResumeTurn`); the main.rs arm now uses it instead of the dead
`StartTurn` check. Pinned by
`retry_dismisses_the_manager_only_when_it_resumes`: blank input
closes the modal and resumes, a draft keeps it open with the
warning.
