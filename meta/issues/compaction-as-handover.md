# Compaction as handover, with the archive one grep away

## Summary

Compaction keeps a recency window of roughly `trigger/3` tokens and
summarizes everything older. Two problems, one measured and one
structural.

**Measured.** The window is chosen with a prose heuristic —
`chars/4 + 2` per event — and applied to content that is often not
prose. In session `efd44d8a` the same code with the same constants
reclaimed 56% at one compaction and 39% at the next:

| compaction | window it meant to keep | provider counted | error |
| --- | --- | --- | --- |
| event 1136 | ~84,652 | 89,719 | 1.06× |
| event 1233 | ~81,398 | 147,677 | 1.81× |

The second window held a 102,476-character block of hex digests and
40k of ANSI-coloured disassembly, which tokenize near one token per
two characters. So it kept 148k when it meant to keep 77k, and the
turn regrew to 237k of a 272k window within the same turn.

**Structural.** The window keeps whatever happened to be *recent*,
which is not the same as what matters. A hex blob survives; the
reasoning that explained it does not. And because dropped context was
gone forever, the heuristic had to be generous — which is why the
window was large enough to have the measured problem at all.

That last clause is the one that changed. With the session's own
archive searchable, dropping context is no longer irreversible, so the
window can go entirely. After a compaction the model gets the system
prompt, its tools, and one summary. Everything else it can look up.

## Requirements

### The archive

- A `history` tool searches the current session's own log — every
  event, including everything compaction dropped — returning bounded
  excerpts with event indices, plus `event N ± k` to read around a
  hit. *(Done: commit 6459dda.)*
- Filterable by speaker, and able to list the user's own messages
  without a query: "what was I actually asked?" should be one call,
  which is what makes carrying the request verbatim unnecessary.
- Scoped to the invoking session, matching the resume guard.
- The `todo` tool becomes readable. It is write-only today, so the
  model's only view of its own plan is the echo in the transcript —
  which is exactly what compaction deletes. State the model can query
  needs no pinning.

### The handover

- After compaction the context is the system prompt, the tools, and
  the summary. No recency window, no verbatim pins, no tail, mid-turn
  included. The only exception is structural: a turn-boundary
  compaction keeps the message that just arrived, because that is the
  request being served rather than history.
- The cut is therefore always "everything before this point", which
  removes `recent_steps_cut`, `event_tokens` and the whole token
  estimate from the cut path. The trigger keeps using the provider's
  reported count, which is ground truth.
- The template carries the load: objective, work state, next move,
  relevant files, the plan, and a note that the archive is searchable.
  It also asks the model to say what it chose *not* to carry, so the
  next turn knows something is there to look up rather than not
  knowing what it is missing.
- No content policing beyond failure detection. A summary that is an
  apology or a refusal — the model answering the conversation instead
  of summarizing it — is reported as an error and the session is left
  untouched. No retry, no fallback, no repair: show the error.

## Acceptance Criteria

- Tests pin the tool changes: speaker filtering, listing user
  messages, and reading the current todo list.
- Tests pin the handover: after compaction the transcript is the
  summary alone (plus the just-arrived user message at a turn
  boundary), and no tool output survives.
- A test pins that a degenerate summary surfaces as an error with the
  session unchanged.
- The full suite passes.

## Notes

- Prior art: Codex's `build_compacted_history` replaces history with
  user messages plus the summary and keeps no tool output. This goes
  further — no verbatim user messages either — which is only
  defensible because of the retrieval tool, which Codex lacks.
- Supersedes [the ruler issue](compaction-ruler.md) outright: with no
  token-budgeted window there is no estimate in the cut path at all.
- Shares its scanner with
  [session search](search-across-sessions.md). *(Done: commit
  e93ae38.)*
- Mid-turn compaction was the worry: the model may be part-way through
  an edit. Decided against special handling. For edits the filesystem
  is authoritative and readable, every turn is checkpointed, and
  anything genuinely unclear is a `history` query. Special-casing it
  would mean a second policy to maintain for a case the design already
  covers.
- Watch for models under-using retrieval; they would rather guess than
  call a tool. That is a prompt-and-measure problem, and the most
  likely way this disappoints.

## Milestone

11 — Beyond the terminal
