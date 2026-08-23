# Rewind and fork in the TUI

## Summary

Surface rewind and fork-at-a-point: a picker over the session's user
turns, `/rewind` and `/fork` slash commands, palette entries, and the
guard rails a destructive restore needs.

## Requirements

- A turn picker modal listing the session's turn boundaries, newest
  first: relative time, an excerpt of the user message, and a marker
  when the turn has a tree checkpoint. Built like `SessionPicker`
  (filter query, `nav_delta`, `ModalHit`).
- Enter rewinds, armed-then-confirm (the `pending_delete` pattern:
  second Enter fires; any navigation or filter edit disarms). The armed
  row states what will happen — how many turns are discarded and
  whether the tree is restored.
- Ctrl-Y forks at the selected turn and switches to the fork. No
  confirmation, matching the session picker's fork.
- `/rewind` and `/fork` in `BUILTIN_SLASH_COMMANDS` and as palette
  entries. Bare `/fork` forks the whole session (existing behaviour,
  now reachable without the session picker); both otherwise open the
  turn picker.
- The switch guard triple applies before rewind or fork: no running
  turn, no background agents, no unsent draft. Rewind additionally
  refuses while a goal is armed (the goal's context is being cut away).
- After a rewind the transcript rebuilds through the existing
  session-switch path with the same session id; a notice reports what
  happened ("rewound N turns · tree restored" / "· no tree snapshot").
- Help overlay and README document the keys and commands.

## Acceptance Criteria

- Pure decision logic (which events are offered as cut points, excerpt
  truncation, armed-state transitions) is unit-tested in the TUI crate.
- Turns without a checkpoint are offered with the conversation-only
  marking, not hidden.
- Guard violations produce notices, not partial state changes.
- The full suite passes; a manual smoke run of rewind and fork on a
  real session is recorded in the DEVLOG.

## Notes

- Services are dropped by the rebuild, consistent with session
  switching; the help text should say so.
- No persistent "rewound here" transcript row: after a rewind the
  transcript is exactly what it was at that point; the notice carries
  the news.

## Outcome

Landed with all acceptance criteria met, including the tmux smoke run
recorded in the DEVLOG. Review caught the one real hazard: /rewind and
/fork typed during a running turn were routed through the steer path as
literal model text; they now join /compact in decide::submit's
maintenance carve-out. Fork verifies its target user-message id against
a fresh load like rewind does, and the rewind notice carries the
discarded-turn count. The unsent message survives the session rebuild
as an input prefill (AppExit::SwitchInto carries prefill + notice
across the teardown).

## Milestone

8 — Time travel
