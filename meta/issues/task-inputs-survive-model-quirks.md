# Task inputs survive model quirks

## Summary

GLM-5.3 habitually writes the *string* "null" for optional task
fields on first attempts: `"task_id": "null"`, `"model": "null"`,
`"reasoning": "null"` (terranigma session — three failed round
trips before it learned by error message). ilar takes the strings
literally: tries to resume session "null", validates variant
"null". Separately, the variant error says "validating inherited
subagent reasoning variant" even when the variant came explicitly
from the task input — misleading for the model trying to correct
itself.

## Requirements

- The task tool treats the exact strings "null" and "" in its
  optional fields (model, reasoning, task_id, workspace) as absent,
  with a comment naming the model quirk that motivates it.
- The variant-validation error names the actual source: the task's
  `reasoning` input when explicit, "inherited from parent" only
  when actually inherited.

## Acceptance Criteria

- A task call with `"task_id": "null"` and `"reasoning": "null"`
  spawns as if the fields were omitted.
- An explicit bad variant errors with wording that names the
  `reasoning` input.

## Milestone

12 — Health sweep

## Outcome

One `is_unfilled` predicate ("null"/blank → absent) wired via
serde onto task_id/model/reasoning and the workspace object;
required fields untouched. Variant errors now name their source —
"from the task's reasoning input" vs "inherited from parent" — and
list the model's valid variants (or say it takes none / is not
cataloged). (97f5d92)
