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
reasoning that explained it does not. And because dropped context is
gone forever, the heuristic has to be generous, which is why the
window is large enough to have this problem at all.

Give the model a way to reach back into its own session and dropping
context stops being irreversible — at which point the window can go
entirely. Compaction becomes a handover: system prompt, the user's
verbatim requests, and a summary that says what was done, what is
next, and that the rest is searchable.

## Requirements

### The archive

- A `history` tool searches the current session's own log — every
  event, including everything compaction dropped — and returns bounded
  excerpts with event indices, plus a way to fetch `event N ± k` for
  context around a hit.
- Excerpts are bounded: a tool that returns a 100k blob has recreated
  the problem with extra steps.
- Scoped to the invoking session (and its subagent children), matching
  the resume guard.

### The handover

- A compaction mode that replaces history with system prompt, the
  pinned verbatim user requests, and the summary. No recency window.
- The summary template teaches the model that the archive exists and
  is searchable, or the model guesses instead of retrieving. This is
  the single biggest determinant of whether the mode feels good.
- Mid-turn compaction is the hard case: the model may be three of five
  call sites into an edit. The `Work State / Active` and `Next Move`
  sections carry that, and a mid-turn summary with an empty `Active`
  is rejected the way a degenerate summary already is.
- Selectable: `compaction.mode` keeps the windowed policy available. A
  bad handover on a six-hour session has no escape hatch, and having
  the old path lets the two be compared on real work.

## Acceptance Criteria

- Tests pin the tool: a phrase only present in dropped history is
  findable, excerpts are bounded, indices address the event, and
  another session's log is not reachable.
- Tests pin the handover: after compaction the transcript is exactly
  system prompt, pinned requests and summary; no tool output survives;
  a mid-turn summary without an `Active` section is rejected.
- A test pins that `compaction.mode` selects the policy.
- The full suite passes.

## Notes

- Prior art: Codex's `build_compacted_history` already replaces
  history with user messages plus the summary and keeps no tool output
  at all. What is new here is the retrieval tool, which is what makes
  the aggressive cut defensible rather than lossy.
- This supersedes calibrating the token estimator
  ([the ruler issue](compaction-ruler.md)): with no token-budgeted
  window there is no estimate in the cut path, and the trigger already
  uses the provider's reported count. Reassess after measuring.
- Shares a scanner with
  [session search](search-across-sessions.md): one walk over session
  JSONL, two front doors — a tool for the model scoped to one session,
  a modal for the user scoped to all of them. Build the scanner once.
- Watch for models under-using retrieval; they would rather guess than
  call a tool. That is a prompt-and-measure problem, but it is the
  most likely way this disappoints.

## Milestone

11 — Beyond the terminal
