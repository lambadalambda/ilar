# Proportion prompts for GPT

## Summary

gpt-6.1-sol applies every process rule to every change and invents
gates of its own. An audit of the tibsun, ascendancy-rs-2, millenia and
emperor sessions (2026-11-14) found:

- About as many docs/process lines as source lines.
- Tests that are larger than the code they test.
- Hash and provenance records that nobody asked for.
- "Blocker" returns instead of marked approximations.
- Parent briefs that become lists of 15 prohibitions.

Codex's GPT-6 prompt has anti-ceremony rules that ilar's patch does not
have. Two of ilar's own prompts make this worse:

- The goal-mode kickoff makes "building a verifier" part of every goal.
- Compaction keeps every constraint forever, including the ones the
  agent set for itself, so the "Do not" list only grows (2 → 28 in
  f5d60354).

## Requirements

- Extend the GPT patch with four sections:
  - Proportion: a rule's purpose decides when it applies; skip a rule
    that costs more than it protects, and say so.
  - Tests: no tests that mirror the implementation; tests normally no
    larger than the code; broad suite once.
  - No invented process: no hashes, provenance, tiers, gates or
    disclaimers that nobody asked for; no new process rules in
    AGENTS.md, issues or notes.
  - Missing facts: approximate and mark instead of returning a blocker;
    a reading budget before the first edit.
- In the Subagents section: briefs give the goal and only the
  constraints the task can break.
- Goal kickoff (all models): use the existing checks; build a new one
  only when the goal cannot be checked otherwise, and keep it small.
- Compaction carry-forward (all models): user directives verbatim;
  self-set constraints are dropped once they no longer apply;
  duplicates are merged.

## Acceptance Criteria

- Unit tests cover each new patch section, the new goal kickoff
  wording and the new carry-forward wording.
- Comparison runs (old vs new prompts, isolated in Podman) on one
  representative orchestrated porting task, reported in DEVLOG.

## Notes

- Audit scripts: /tmp/solaudit/ (not tracked).
- Follows [A channel prompt patch for GPT](a-channel-prompt-patch-for-gpt.md).
