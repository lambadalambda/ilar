# Accurate tool and subagent activity

## Summary

Running tool rows conflate provider argument streaming, scheduler waiting, and execution. Completed tool inputs can appear to be "waiting for provider," while delegated tasks look like generic stalled tools with static received-byte counters.

## Requirements

- Distinguish provider argument streaming from completed calls waiting for scheduler admission.
- Do not describe queued or executing local tools as waiting for provider data.
- Present delegated `task` calls as subagents with a distinct label and useful identity.
- Keep subagent description and configured agent identity structured rather than recovering them from a display summary.
- Mark foreground subagents active only after their internally managed workspace lease is acquired.
- Show active subagents with elapsed time even when their tool-input byte count is static.
- Preserve bounded row widths, restored-session behavior, and existing tool lifecycle semantics.

## Acceptance Criteria

- A completed tool call transitions out of the receiving/provider state before scheduler execution begins.
- Queued local tools have explicit scheduler-waiting presentation.
- Running task rows are visibly identifiable as subagents and include their description or configured agent when available.
- Serialized mutable task rows remain queued until each task actually acquires its workspace.
- Running task presentation uses its runtime activation timestamp rather than implying received bytes are execution progress.
- Focused lifecycle regressions and full workspace checks pass.
