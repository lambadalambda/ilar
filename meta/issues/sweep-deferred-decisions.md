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
