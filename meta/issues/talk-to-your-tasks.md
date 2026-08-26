# Talk to your tasks

## Summary

Approved 2026-08-26: one uniform verb for messaging a subagent,
with ilar's own steering semantics. Half exists already —
`task(task_id, prompt)` resumes a completed child from its
transcript. The missing half is the running case: deliver a
message to an in-flight child at its next step boundary, the way
root-turn steers already work. Modeled on the orchestrator-harness
pattern discussed in session (message queued at the next tool
round; resume-from-transcript for stopped agents), but keeping the
visibility ilar's steering has and that pattern lacks: a pending
strip showing the steer's fate.

## Requirements

- Messaging a RUNNING task steers it: the text is delivered at the
  child turn's next step boundary via the existing steer channel
  machinery, wired into child `run_turn` calls (foreground and
  background; notification-routed turns too if reachable).
- Messaging a FINISHED task resumes it from its transcript —
  today's `task_id` resume, presented as the same verb. The model
  should not need to know which case it is in.
- Surface: either the task tool gains a message mode or a sibling
  `task_message` tool — pick whichever reads better in the schema,
  one verb total, documented with the decision tree.
- Visibility: an undelivered child steer appears with its fate
  (pending strip / agents panel), and moves to the child's queue
  rather than vanishing if the child's turn ends first — mirror
  the root steer rules.
- The TUI's agents panel pointing a user steer at a child directly
  is out of scope here (note it as the follow-up).

## Acceptance Criteria

- A steer sent mid-child-turn is seen by the child at its next
  step (fake-provider test); one sent as the child stops lands in
  its resume rather than vanishing; a message to a finished task
  resumes it with context intact.

## Milestone

13 — Guard rails
