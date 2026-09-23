# Words a model acts on, again

## Summary

Model-facing text that points at the wrong next step or contradicts
another text, found in the 2026-09-23 sweep. Siblings of
[[words-a-model-acts-on]].

## Requirements

- The queued-message notes invite a resend, and `ChildSteers::queue`
  (subagent.rs:458) does not dedupe, so the child reads it twice; and
  nothing says who resumes the task (nothing does, automatically).
  Say "read when you next resume this task (task_message 'continue');
  do not resend", and/or dedupe identical queued text.
- "aborted" and "cancelled" are one event with two words
  (`headline` ~2900/2901; the hop maps Aborted to cancelled). Fold.
- The base prompt says "You have tools: read, write, edit, bash, glob,
  grep" (config/agents_md.rs:7) to every agent, including explore and
  review. Drop the list.
- The compaction prompt says "Nothing here is lost" (compaction.rs:75)
  and "anything you do not carry forward is lost" (:89).
- The abnormal-ending notification has no task id (subagent.rs ~3289).
- Resume refusals: "already active; wait for it to finish" (~1116)
  invites polling — name task_message; "provide its explicit
  workspace" (~1163) — name the path, or task_message.
- `tasks` says pass task_id "to give a finished task a fresh scope"
  (~3923), `task` says "follow-up questions on the same scope" (~3683).
- "retry after a notification is handled" (~1066, ~1936) is not
  something a model can do.
- "Deferred background task started" — not deferred.
- `run_in_background` on bash vs `background` on task; `TaskInput`
  ignores unknown fields, so `run_in_background: false` on a task is
  silently detached. Deny unknown fields or accept the alias.
- "Background job {id}" ids name nothing a tool takes; name the
  command.

## Acceptance Criteria

- Each rewrite pinned where a test already pins the old sentence.
- Full gate green.

## Notes

- Source: UX sweep 2026-09-23 (model words pass). Size: S-M.

## Done (2026-09-23)

In `426e902`: the queued notes say who resumes a task and not to
resend, and an identical unread message is queued once; a task's
abort is "cancelled" everywhere (`TurnEnding::Aborted` stays readable
for old logs); the base prompt no longer lists six tools to agents
that have others; compaction says "drops out of sight", not "lost";
the abnormal ending names its task id; the resume refusals name
`task_message` or the worktree path, and say when there is none; the
tasks tool and the task tool agree on `task_id`; the capacity refusal
says what to do; "Deferred" is gone; `run_in_background` is accepted
for `background` on task and task_message.

Kept: "Background job job-1" — the TUI and the share page parse the
id out of that line.
