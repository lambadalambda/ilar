# A slow call does not hold the step

## Summary

A step waits for the slowest call it issued. A five-minute build
issued beside a grep keeps the model idle for five minutes with the
grep result already there, and a message the person sends meanwhile
is a steer, read only at the next step boundary. `bash
run_in_background` and `service` avoid this, but the model has to
choose them in advance.

Unreal Agent (unreallabs.ai/blog/unreal-agent, 2026-09-22) makes every
call asynchronous: a placeholder result at once, the real one appended
when it lands, the model woken per completion. Their placeholder is a
second `function_call_output` for the same call id, which they report
some non-OpenAI providers reject — not an option on our wires. The
shape that fits ilar: a foreground `bash` that outlives a threshold
turns into a background job, the step gets "still running; the result
comes as a notification", and the existing notification path carries
the result.

## Requirements

- First, measure on the session logs whether this pays: how much wall
  time is spent in steps held by a slow call, which tools hold them,
  and how often a slow call shares its step with fast ones.
- If it pays: a foreground `bash` past a threshold detaches into a
  background job without the model asking, keeping its output tail.
- The detached result arrives as the same notification an explicit
  `run_in_background` gets.
- The step's result for that call says it is still running and how
  the result will arrive.

## Acceptance Criteria

- The measurement is recorded here with its method.
- A test: a foreground bash past the threshold returns a
  still-running result, and its completion arrives as a notification.
- Full gate green.

## Notes

- Codex's `exec_command` has `yield_time_ms`, but then the model polls
  with `write_stdin`; ours would notify instead.
- Mutating calls still serialise; a detached bash that writes keeps
  whatever lease it held.
- Source: user request, 2026-09-23. Size: M.

## Measurement (2026-09-23)

Every session log on the Mac, 2,833 sessions, 142,564 steps with tool
calls. The log writes a step's results together, so a step's wall time
(response to last result) is measurable and a single call's is not.
Steps with `question` or `sudo` are left out (they wait on the
person), and so are steps over 30 minutes in the second cut (a
sleeping laptop or a restart).

**Bash alone rarely holds a step.** In the 2,375 sessions that never
delegated, 185 of 50,635 steps took 30 s or more: 6.5 h of about
307 h active. Only 7 of those had a fast call beside the slow one, and
no message from the person followed one. A long bash step's median is
61 s. Auto-detaching bash would buy almost nothing here.

**The holds are on the subagent side.** Across all sessions, steps of
30 s or more add up to 515 h:

- foreground `task`: 1,346 steps, 266 h — the old default, gone since
  `9813e02`;
- `task_message`: 785 of its 1,375 steps took 30 s or more, median
  43 s. Messaging a *finished* task resumes it with
  `background: Some(false)` (subagent.rs, `message_task_outcome`), so
  the parent waits for the whole resumed turn;
- bash behind a child's write lease: single-bash steps held 114 h in
  all sessions against 6.5 h without delegation, with cheap commands
  among them (`git status && git diff --check` held 629 s, `sleep 30`
  held 1,050 s). With mutable tasks now in the background by default,
  this wait gets more common, not less.

354 messages from the person came right after a step of 60 s or more,
all in sessions that delegated.

Also seen: 237 steps (12 h) where the model slept to poll something
outside ilar — a remote job over ssh, a `nohup` log. That is the
model's choice, and detaching would not remove it.
