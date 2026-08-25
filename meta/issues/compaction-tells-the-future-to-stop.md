# Compaction tells the future to stop

## Summary

The summarization instruction opens with "Stop working on the task."
scoped to nothing — an obedient model carries that stance into the
handover (e.g. a Next Move of "stop working", or framing the
objective as ended), and the post-compaction turn reads the summary
and stops working, observed in the wild. The stop applies only to
the checkpoint turn itself; the task continues afterward.

## Requirements

- The instruction makes the scope explicit: pausing is for this
  checkpoint only, the task resumes immediately after the handover.
- The summarizer is told to address an agent who resumes work — and
  never to instruct that agent to stop, wait, or seek confirmation
  the conversation didn't ask for.
- The injected `<compaction-summary>` message tells the resuming
  turn to continue the work rather than standing bare.

## Acceptance Criteria

- Instruction-content tests pin the scoping language (same idiom as
  the existing carry-forward test).
- A store test pins the continue framing on the injected summary
  message.

## Milestone

12 — Health sweep

## Outcome

Three-sided fix: the instruction scopes the stop ("for this one
turn only … the task itself resumes immediately afterwards"), a new
rule forbids the summarizer from retiring its reader ("a handover,
not a sign-off … Never tell them to stop, wait, or seek
confirmation"), and the injected message now ends with "Continue
the task from this state — the checkpoint replaced the earlier
conversation, not the goal", so stop-flavored wording inside a
summary never gets the last word. Pinned by
`the_stop_is_scoped_to_the_checkpoint_not_the_task` and
`injected_summary_tells_the_next_turn_to_continue`.
