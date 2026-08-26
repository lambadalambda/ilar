# Edits gate on what the model has seen

## Summary

Weaker models (observed: GLM-5.3, terranigma session) edit from a
stale mental copy of the file — including after mutating it via
bash out-of-band — and loop on bare "old_string not found" errors.
Three measures, approved 2026-08-26; the indentation-tolerant
fallback was considered and explicitly rejected (it weakens the
exact-match contract).

## Requirements

1. **Hash-gated edits.** The session's tool context tracks
   path → content hash (canonicalized paths), updated by read
   (whole-file hash regardless of offset/limit window), write, and
   edit. Edit fails two distinct ways: no entry — "you have not
   read this file in this session; read it first"; hash mismatch —
   "the file changed since you last read it (a command or another
   process wrote it); re-read before editing". Content identity,
   not wall-clock. Subagents start with an empty map (the child
   model has seen nothing). Compaction clears the map: the summary
   truncated the model's memory of file contents, so the first
   edit after a compaction must re-read. Write stays ungated for
   now (whole-content, no stale-match risk) — note only.
2. **Nearest-match diagnostics.** On no-match, find the closest
   region (whitespace-normalized comparison, bounded cost on large
   files) and include it verbatim in the error with its line
   numbers, so the model corrects in one round trip. When nothing
   is remotely close, say that instead.
3. **Line-number contamination detection.** old_string or
   new_string lines matching read's `N→` prefix produce a specific
   error naming the field and telling the model to copy the text
   without the prefix — checked before the generic no-match path.

## Acceptance Criteria

- Red-first tests per behavior: unread-file edit refused; edit
  after out-of-band bash mutation refused with the changed-since
  wording; edit after re-read succeeds; compaction forces a
  re-read (agent-loop level); nearest-match error shows the
  drifted region with line numbers; `N→` contamination named
  specifically for each field.
- The gate's errors must be actionable enough that the existing
  suites' models-eye-view stays coherent (error text tests).

## Milestone

13 — Guard rails

## Outcome

`SeenFiles` (Arc-shared per session, canonical paths, SHA-256,
16 MiB cap aligned with edit's own limit) records on text reads
(whole-file hash even for windowed reads; binary/image reads never
license edits), writes, and successful edits; children start blind;
the map clears when the *identity* of the latest compaction event
changes — a count would freeze at 1 because checkpoint rebuilds
drop all but the newest Compaction (caught in review, verified
red). Edit refuses unread and changed-since files with distinct
actionable wordings. No-match errors show the nearest
whitespace-normalized region with line numbers (bounded: 20-line
probe, 5k-line scan, 400-byte line cap) or say nothing is close.
`N→` contamination is named per field on the no-match path, and the
destructive variant — clean old_string matching, prefixed
new_string — is refused pre-write under the asymmetric rule (two
adjacent consecutive-numbered lines in new only; symmetric
prefixes and single cited examples still edit; write stays the
escape hatch). 29+5 tests. Accepted nit for later: the hash is a
second full read pass — incremental hashing off the open reader
would also close a narrow TOCTOU.
