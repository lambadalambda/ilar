# A foreground task does not wait out a detached one

## Summary

After [[a-held-checkout-answers-at-once]], one wait was left: a mutable
`task` or `task_message` passed `background: false`, in a checkout a
detached job holds, waits in the lease for as long as that job runs —
holding the model's step and every message from the person with it.
`false` says the model is blocked on this task, not on another job.

## Requirements

- Refused at once, naming the holder, with the two ways out: leave
  `background` out to queue behind it, or a worktree.
- A holder inside a step (a foreground sibling) is still waited for;
  holder marks say which kind they are.
- A resume the person starts from a task's view still waits: it holds
  no step.

## Acceptance Criteria

- A test: behind a background bash job, `background: false` is refused
  quickly and names the job; left detached, the same task starts.
- Full gate green.

## Notes

- Source: user decision, 2026-09-23. Size: S.
