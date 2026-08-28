# Steers outlive a turn that declines

## Summary

Found 2026-08-28 by audit. The parent→child steering half of the
subagent system is materially weaker than the completion half. Two
confirmed loss paths for the parent's *instructions*:

**The `started()` call is premature.** Both task resume sites and
`route_notification` call `ChildTurnSteer::started()` — which
clears the queue so drop restores nothing — as soon as `run_turn`
returns anything but `WouldBlock`. But `run_turn` has five ways to
fail *before* it appends the prompt: the writer acquire, variant
options, provider resolution, and two appends. Any of those →
the queued messages existed only in the prompt string → gone. Most
reachable trigger: `task_message` resuming a child whose model's
provider config was removed — the parent's message is silently
eaten and the error only mentions the provider. (The comment
"nothing after this point declines to start" is factually wrong,
and `route_notification`'s own comment promises a restore that
only the `WouldBlock` arm performs.)

**`message_task`'s resume branch drops the message on every early
return.** Its two sibling branches (steer a live turn, queue for a
turn about to start) both park the text durably in
`ChildSteers::pending`. The third moves it into `TaskInput.prompt`
and delegates into `run_task_observed`, which has ~20 early error
returns — depth cap, unknown agent, session already active (the
exact TOCTOU the queue branch exists for), and the concurrency
limit, whose error text is "Do not retry": the message is
destroyed *and* the model is told not to send it again.

## Requirements

- `started()` means "the prompt was appended", not "run_turn was
  called". Either run_turn signals that boundary, or the caller
  restores the queue on any error that provably appended nothing.
- `message_task`'s resume branch parks the text in the pending
  queue before delegating, like its siblings, so an early return
  leaves the message waiting instead of gone.
- The concurrency-limit rejection must not claim finality for a
  message it dropped.

## Acceptance Criteria

- A `task_message` against a child that hits the concurrency limit
  (or any resume-validation error) leaves the message queued and
  says so; a later resume delivers it.

## Milestone

13 — Guard rails
