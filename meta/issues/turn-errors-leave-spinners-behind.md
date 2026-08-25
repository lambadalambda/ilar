# Turn errors leave spinners behind

## Summary

`finish_turn`'s error path (app.rs:1170-1176) loops over
`self.lines` flat: it fails tool rows but never clears
`child_running` or recurses into `child_lines` — a partial
copy-paste of `close_running_tools` (app.rs:3091), which does both
and is only called on abort. Because the renderer forces a running
display while `child_running` is set, a provider error during a live
Task leaves the agent row spinning forever. `Completion::Crashed`
(schedule.rs:285-292) is worse: no transcript cleanup at all —
spinners and incomplete streaming thoughts persist on an idle app.

## Requirements

- The error path calls `close_running_tools` instead of its partial
  copy.
- The crashed path performs the same teardown as the error path
  (prune incomplete thoughts, close running tools).

## Acceptance Criteria

- Tests: a turn error with a running child clears `child_running`
  everywhere; a crash leaves no running tool rows or incomplete
  thoughts.

## Milestone

12 — Health sweep

## Outcome

Both paths now share `App::close_open_rows` (prune incomplete
thoughts + the recursive `close_running_tools`): `finish_turn`'s
error arm calls it instead of its partial flat copy, and
`Completion::Crashed` calls it too — a crash delivers no `TurnDone`
and no error event, so this is its only cleanup point. The stray
doc comment that had drifted onto `MAX_IMAGE_DIM` is back on
`close_running_tools`. Pinned by
`a_turn_error_stops_the_subagent_spinner` and
`a_crash_closes_what_the_turn_left_open`.
