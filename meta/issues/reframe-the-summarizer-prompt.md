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
### Prior art

opencode (`packages/core/src/session/compaction.ts`) uses a fixed
Markdown template whose first section is `## Objective` — "one or two
brief sentences describing what the user is trying to accomplish" —
followed by Important Details, Work State (Completed / Active /
Blocked), Next Move, and Relevant Files, with every section kept even
when empty. Its rules include "Preserve exact file paths, symbols,
commands, error strings, URLs, and identifiers when known" — the exact
rule whose absence dropped two PR links here.

Its second prompt is the more interesting one. When a prior summary
exists, opencode tells the summarizer outright: "The <prior-summary>
is discarded after this: anything you do not carry into the new
summary is lost", and to carry forward objectives, constraints, user
directives, decisions and parallel workstreams even when the recent
conversation does not mention them. ilar chains summaries the same
destructive way — `transcript_of` emits only the newest — but never
says so, so a second compaction can quietly drop what the first
preserved.

Codex frames its prompt as a "CONTEXT CHECKPOINT COMPACTION… handoff
summary for another LLM that will resume the task", asking for
progress and key decisions, constraints and user preferences, what
remains, and "any critical data, examples, or references needed to
continue".

Both are objective-first handovers. ilar's is a work log.

## Milestone

10 — Everyday polish
