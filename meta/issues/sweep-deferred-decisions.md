# Sweep deferred decisions

## Summary

The health sweep's residual items that are genuine decisions or
need live probing — carried out of sweep-cleanups when its batch
closed. Each needs a call, not just work:

- **Subagent residuals** (977ceab): `child_ctx.cancel` in the
  background branch is the root token — tools inside an aborted
  background child never see cancellation (one-liner, observable
  change); `spawn_background_tool` hand-rolls the shared select
  shape; `revalidate_after_lease_for_session` is a deletable alias.
- **Pause-path leftovers**: mostly mooted by the pause-machinery
  removal (be56531) — verify nothing remains, then drop this line.
- **`ModelAccess::Zai` catalog rows** (glm-4.6, glm-5.1, glm-5,
  glm-4.5*): unlistable since only the coding route exists — probe
  the coding endpoint per model, then re-tier or prune.
- **`ContentBlock::Thinking.signature`**: always `None` now — keep
  for a future native-Anthropic provider or drop.
- **Uncataloged zai models**: wire reserves the 16k output floor
  but the config fallback path reserves nothing.
- **Live tool detail vs restored** (from cleanups): turn.rs caps
  raw text, the TUI caps tab-expanded text — same limit and marker,
  but tab-heavy output can cut at a different character.

- **`SubagentSpawner` project-instructions default**: defaults to
  `Include`; a future call site that forgets
  `.with_project_instructions` silently re-includes a refused file.
  Making it a required constructor argument touches ~20 test sites.

## Milestone

12 — Health sweep

## Outcome

All decided and landed (aae6cf5). Catalog: seven models retiered to
ZaiBoth on live probe evidence, glm-4.7-flashx pruned (error 1113 on
the plan), the memberless `Zai` variant deleted — ZaiBoth won over
ZaiCodingPlan because the pair distinguishes pricing display, and
these models have list prices like their peers. Background-child
cancellation: the sweep's premise was stale — run_turn already
overwrites the context token with the task-scoped one, so tools did
see aborts; the assignment is now consistent anyway and an
end-to-end bash-tool cancellation test guards it.
spawn_background_tool shares the child-token shape; the revalidate
alias is inlined; `Thinking.signature` (and its ProviderEvent twin)
dropped with a test proving the three signed legacy logs still
load; the dead announced-calls loop deleted; `project_instructions`
is a required `SubagentSpawner::new` argument (setter deleted);
live/restored detail cuts unified — the real divergence was image
markers, fixed and red-tested. Verified moot: the contentless-pause
usage drop and the uncataloged-zai asymmetry (nothing sends
max_tokens). Accepted residuals, recorded here and nowhere else:
the 11-line answer-unrun-calls shape has three copies in turn.rs;
abort/error turns publish no StepComplete so live totals can lag
restored ones in two narrow races; a `plan_billed` row flag could
replace ModelAccess's billing overload; a `SubagentConfig` struct
would fix the /nonexistent user_config_dir footgun; the display
bound is now 16k raw chars (tab-dense results render longer).
