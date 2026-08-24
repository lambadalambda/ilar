# Reframe the summarizer prompt

## Summary

`SUMMARIZER_PROMPT` asks for "tasks attempted and their outcomes,
decisions made, open questions, important file paths, and user
preferences". Every item is retrospective: it describes a work log.
Nothing asks for the objective currently being served, and nothing
asks for identifiers to be copied rather than paraphrased.

The summary in session `9860bd12` is a good work log and a poor
handover: it opens "Implemented bundled-payment support across
Firehose and its producers", never states what was asked, and drops
both PR links it was given as source material.

## Requirements

- The prompt leads with the objective in the user's own words, then
  the state of the work.
- URLs, PR numbers, branch names, worktree paths, file paths and
  command names are copied verbatim, never paraphrased or summarized
  away.
- Explicitly ask for what is *not* done and what was ruled out —
  a summary that only records successes invites repeating rejected
  approaches.
- Keep it dense; this is a prompt for a summarizer, not a template.

## Acceptance Criteria

- The prompt states the objective-first ordering and the
  copy-identifiers-verbatim rule.
- A test asserts the prompt covers objective, remaining work and
  verbatim identifiers, so a future edit cannot quietly drop them.
- The full suite passes.

## Notes

- Pairs with [pin user requests through compaction](pin-user-requests-through-compaction.md),
  which makes the objective's survival structural rather than
  prompt-dependent.
- Worth comparing against what opencode and Codex do before settling
  the wording.

## Milestone

10 — Everyday polish
