# A held checkout answers at once

## Summary

A parent's `edit`, `write` or `bash` that finds its checkout held by a
detached mutable task or a background bash waits until that job
reports. The step holds, and a message from the person waits with it.
In the logs, single-bash steps held 114 h in sessions that delegated
against 6.5 h in those that did not, with `git status` among them
([[a-slow-call-does-not-hold-the-step]]). With tasks detached by
default, this is the step that holds now.

Inside one step no call can hold the lease against another: `bash`,
`edit` and `write` are barriers. So whoever holds it when the executor
asks is detached, and waiting for it is waiting for something the
model already chose not to wait for.

## Requirements

- In the executor, a mutating call whose checkout lease is taken
  returns an error at once, and nothing runs.
- The error says the checkout is held by a detached job of this
  session until it reports, that its completion wakes the model, and
  what to do meanwhile: read, glob and grep; `tasks` lists what runs;
  end the turn to wait. Not to retry until then.
- A detached job's own wait for the lease is unchanged: mutable tasks
  sharing a checkout still serialise.
- The task tool text and the agents doc say the new rule instead of
  "your edit, write and bash calls wait".

## Acceptance Criteria

- A test: with the lease held, a plain mutating tool and a lease-taking
  tool both return the refusal without running.
- Full gate green.

## Notes

- A foreground `task` (`background: false`) that finds the checkout
  held still waits: the model said it is blocked on that result.
- Source: measurement, 2026-09-23. Size: S.
