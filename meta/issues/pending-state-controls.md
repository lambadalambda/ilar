# Pending-state controls: Esc ladder + pending manager

## Summary

Esc was overloaded across state of different lifetimes (abort turn,
cancel background jobs, clear input, drop queue, abort goal): a user
trying to remove one queued message aborted a running turn and their
goal, while the stale queued message survived to auto-send later.

## Requirements

- Esc ladder is strictly immediate-scope: close overlay/search > abort
  running turn > clear input. Nothing else; idle-empty Esc is a no-op.
- Pending manager (Ctrl-Q, palette "Pending…"): lists queued messages
  (delete one / Enter edits it into the input), the goal (edit /
  confirmed abort), background jobs (confirmed cancel-all), and the
  retry offer (dismiss / Enter retries).
- Goal abort only via the manager or explicit `/goal abort`; aborting a
  turn mid-goal pauses the goal with a clear notice and it resumes after
  the next completed turn.
- Held queue notices point at the manager.

## Acceptance Criteria

- Tests: manager item derivation and mutation (delete/edit/dismiss),
  confirmed aborts, /goal abort, Esc no longer touching queue/goal.
