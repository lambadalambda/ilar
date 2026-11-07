# Consolidation checks claims against sources

## Summary

Gateway weekly review reorganizes notes and daily summaries, but it has no directed process for checking existing claims against original evidence. A summary can repeat the same mistake as the note.

## Requirements

- Use [note evidence](memory-notes-keep-their-evidence.md) and [scoped transcript retrieval](agents-can-read-scoped-source-transcripts.md) to check a bounded set of claims, initially through the gateway's maintenance path.
- Define candidate selection and limits on model calls, tokens or elapsed time. Prefer important or change-prone claims; do not scan the entire store on every run.
- Distinguish evidence of a past observation from evidence that a claim is still true now. Require a current authoritative check before marking live state current; otherwise leave it unverified or mark it stale.
- Amend contradicted notes and retire disproved claims. Carry corrections into the summary and remove or demote unsupported core entries. Mere repetition across notes is not confirmation.
- Preserve uncertainty when evidence is missing or conflicting. Never invent a verification result.
- Respect memory enablement, private/project scope and the chosen approval policy. Keep model calls outside store locks; skip or re-check a note that changed while validation ran.
- Produce a concise report of evidence checked, edits, unresolved claims and cost. Keep correction logic reusable by terminal maintenance without moving gateway scheduling into core.

## Acceptance Criteria

- Scripted tests cover a wrong note corrected by original evidence, a correct note left unchanged and an unsupported note left unverified.
- A stale-state case is not marked current from an old transcript alone.
- Tests cover a changed summary, an unsupported core entry, missing evidence, budget exhaustion and an intervening user amendment.
- Tests allow authorized private evidence but prevent evidence from crossing project/private scope or appearing in a report delivered outside that scope.
- No automatic rollout occurs before the write-safety fixes in [the parent issue](memory-follow-ups-from-the-component-review.md) are verified; semantic benefit is measured separately.

## Notes

- Current weekly prompt: `crates/ilar-gateway/src/weekly.rs:65-78`.
- Transcript-backed checking is not a guarantee that every transcript claim is true. Preserve the distinction between user statements, observed tool results and model guesses.
- This issue does not add a wiki, embeddings or a blanket archive rewrite.
