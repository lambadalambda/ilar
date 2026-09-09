# A weekly review of memory and skills

## Summary

Hermes's Curator keeps the skill library from sprawling: usage
counts, stale after 30 days and archived after 90 without a model,
and an optional weekly consolidation pass. OpenClaw's "dreaming"
promotes daily notes into the core. With cron in place both are one
scheduled turn with a fixed prompt, plus a deterministic sweep.

## Requirements

- A default cron job, weekly, on a background seat: read the daily
  notes since the last run, promote what recurs into the core memory
  (through the `memory` tool, so the caps hold), retire what is
  stale, and consolidate overlapping skills through `skill_manage`.
  Off with `gateway.weekly.enabled = false`.
- Deterministic skill staleness from the ledger: unused for 30 days
  is stale (listed last, marked), 90 days archived to
  `<home>/skills/.archive/`.
- Everything it changed in one notice to the last active chat.

## Acceptance Criteria

- Tests: the sweep moves a stale skill; the job runs once a week and
  writes through the tools, not around them.

## Notes

- Done. The job is `weekly` in `cron.json`, target `last`, upserted at
  every start from `[gateway.weekly]` (`cron`, `stale_after_days`,
  `archive_after_days`) and removed when disabled. The sweep runs in
  the gateway right before the turn and its findings are appended to
  the prompt. A skill the ledger has never seen is left alone.
