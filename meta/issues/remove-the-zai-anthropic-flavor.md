# Remove the zai Anthropic flavor

## Summary

The z.ai Anthropic-compatible flavor is a second wire dialect
maintained for a provider that also speaks the first: caching works
on the OpenAI route (verified in real sessions), thinking arrives as
`reasoning_content`, and the models are identical. It produced five
of the health sweep's provider bugs (message_start cache loss, the
16k cap, the pause-path trio, vision-blind 4.6v/4.5v) and its one
unique behavior — server pauses — exists only because of the route
itself. Per the user: rip it out with no compatibility vestiges
(single-user project), and remove the turn.rs pause machinery with
it; git history is the resurrection path.

Session-log scan (2026-08-25): zero `provider_replay` blocks and
zero `paused` stop reasons across every session — the variants can
be deleted without parse tolerance.

## Requirements

- zai.rs: `Flavor` gone; only the OpenAI-compatible path remains
  (coding endpoint default). `AnthropicMapper`,
  `anthropic_message`/`anthropic_tool`, and their tests deleted.
- config: the zai `flavor` key gone entirely (an old config with
  the key fails with serde's unknown-field error — acceptable);
  the zai-anthropic `input_limit` branch gone; `max_output_tokens`
  removed if nothing else reads it.
- provider request: `continuations` field gone.
- turn.rs: the pause machinery gone — `paused_content`,
  `paused_usage`, `pause_retries`, `merge_segment_usage`,
  continuation validation/replay, `StopReason::Paused` arms;
  `persist_failed_step` stays for the provider-error path.
  `max_pause_retries` config knob gone.
- `StopReason::Paused` and `ContentBlock::ProviderReplay` variants
  deleted, with every match arm across the workspace.
- Docs mentioning the flavor updated; pause-behavior tests deleted
  with the behavior.

## Acceptance Criteria

- Workspace tests and clippy green; grep finds no
  Anthropic/pause/flavor remnants outside git history and this
  tracker; a zai wire test pins the single remaining shape.

## Milestone

12 — Health sweep

## Outcome

Gone: `Flavor`, the Anthropic wire branch, `anthropic_message`/
`anthropic_tool`, `AnthropicMapper` (+336 lines of mapper),
`max_output_tokens`, `Request::continuations`, the entire turn.rs
pause machinery, `StopReason::Paused`,
`ContentBlock::ProviderReplay`, and the orphaned
`ProviderEvent::ResponseContent`; the config `flavor` key deleted
outright, zai reach reduced to the coding-plan route; four
Anthropic SSE fixtures, 20 wire/mapper tests, 9 pause tests, 3 live
smoke tests removed; docs and ilar.toml.example updated. Net ~1,900
lines gone. A wire test pins the single remaining request shape.
Full workspace green, clippy clean. Follow-ups recorded in
sweep-cleanups: `ModelAccess::Zai` rows are now unlistable (prune
or re-tier), the zai prompt-cache prefix-stability coverage gap,
and the now-always-None thinking `signature` field. (be56531)
