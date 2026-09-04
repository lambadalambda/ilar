# Task rows say what finished

## Summary

The collapsed row a task result wears leads with noise. The headline
normalizer (session_view.rs:64-83) only recognises `" completed.`,
but the producer writes `Task "{description}" completed (task_id:
{session_id}).` (subagent.rs:1235), so every real completion keeps
both the `Task "` wrapper and a raw UUID: `task ▸ Task "Fix tests"
completed (task_id: 7c1e…)`. The pending strip reuses the same
headline. Job rows lead with the job id (`job ▸ job-1 ("Run checks")
completed.`, session_view.rs:89); a propagated `Nested task
completed.` (subagent.rs:1899) names no task at all. Export writes
only the first line of Task/Job/System rows (transcript.rs:2280) and
never recurses into child timelines.

## Requirements

- Headline: `Fix tests completed` / `Run checks completed` — verb and
  description, no wrapper, no id; the id stays in the expandable body.
- Nested results carry the task description they are about.
- Tests use the producer's real strings, not hand-written ones.
- Export emits bodies as blockquotes and recurses into `child_lines`.

## Acceptance Criteria

- A restored and a live completion render the same headline, and it
  contains no `task_id`, no `Task "`, no job id.

Size: S. Source: UX sweep 2026-09-03 (transcript).

## Outcome (2026-09-04)

Headlines now read `Fix tests completed.` / `Run checks completed.` /
`nested: review the diff completed.`; the task or job id moves to the
row's body as `task_id: …` / `job: …`, where the tasks tool and a
follow-up still find it. The normalizer splits on the producer's own
verbs (so descriptions and failure texts may carry quotes) and is
tested against the producer's real strings. The propagated "Nested
task" notes name their task at the source (subagent.rs), including the
two failure hops. Export writes bodies as blockquotes and an agent
row's child timeline in a `<details>` block.
