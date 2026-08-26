# Big tool output spills to disk

## Summary

The "Maintaining a Pleroma Forgejo Fork" session jumped 91k→224k
context tokens in one step: four unfiltered curls to an issue-search
API each filled bash's 100 KiB cap with minified JSON (~30k tokens
apiece), and the truncation note taught the model nothing — it
re-dumped four times. Adopt the opencode pattern (their
truncate.ts, read 2026-08-26): a small model-facing preview, the
full output spilled to disk, and a hint pointing at the file so the
model self-serves with targeted grep/read instead of re-running the
command. Codex's head+tail-with-omission-marker and
model-chosen-token-budget were considered; the spill pattern wins
on behavior shaping and composes with ilar's existing tools.

## Requirements

- Bash preview to the model shrinks to ~30 KiB, still tail-biased
  with the guaranteed stderr share (scale the existing budgets;
  none of the drain/stderr machinery regresses).
- Capture grows past the preview: retain up to a few MiB per
  stream (tail-biased) and, when output exceeded the preview, write
  the captured bytes to `state_dir/tool-output/<call_id>.txt`; the
  result ends with "full output: <path> (N MB, M lines) — grep or
  read it for what you need" (plus raw totals when even the capture
  truncated).
- Retention: spill files older than 7 days are cleaned at startup.
- The bash tool description says filtering at the source (jq, grep,
  head) is still cheaper than reading the spill.
- **grep and glob accept absolute paths**, like read/write/edit
  already do — `ensure_workspace_relative` goes, tool descriptions
  and schemas updated to tell the truth. The restriction protected
  nothing (no sandbox by design, bash exists) and blocked exactly
  this workflow.

## Acceptance Criteria

- A >preview output yields the small tail preview + a hint naming
  an existing spill file whose content is the full capture; grep
  with the spill file's absolute path finds content in it; an
  under-preview output produces no spill file.
- Stderr survival and tail bias pinned at the new sizes.
- Old spill files are removed at startup; fresh ones survive.

## Milestone

13 — Guard rails

## Outcome

Landed per the opencode pattern: preview shrank 100→30 KiB
(tail-biased, stderr half-share preserved), capture grew to 2 MiB
per stream, overflow writes
`state_dir/tool-output/<session>-<call>.txt` and the hint leads the
result — first line, so the TUI's head-biased display shows the
path to the human too, not just the model. Honest raw-totals caveat
when even the capture truncated; write failures degrade to a note,
never an error; 7-day startup sweep. grep and glob accept absolute
paths (`ensure_workspace_relative` deleted, schemas updated), so
the model greps the spill directly — the acceptance test does
exactly that. The Forgejo-session case drops from ~30k tokens per
dump to ~9k, with the full data one grep away. Recorded residuals:
notification-routed turns have no spill dir; spill files are 0644
like session logs; the drain read-buffer now scales with the cap
(perf fix that rode along).
