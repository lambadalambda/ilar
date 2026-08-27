# Grep results spill like bash output

## Summary

Found in the yodlpay Sentry session (2026-08-27): one grep with
`include_ignored: true` over another repo returned 39k chars (~10k
tokens) straight into context. Grep's own caps top out at 200
matches / 256 KiB rendered — 8× the 30 KiB preview discipline bash
learned in the output-spill work. Same cure: past the preview
budget the full match list goes to the spill file and the result
opens with the pointer.

## Requirements

- A successful grep whose rendered matches exceed the bash preview
  budget (30 KiB) writes the full rendering to
  `state_dir/tool-output/<session>-<call>.txt` (the bash naming and
  sweep, shared, not copied) and returns the hint first — path,
  size, line count, "grep or read it for what you need" — followed
  by the head of the matches up to the budget. Head-biased, unlike
  bash: matches are sorted by path, so the front is where the
  grouping starts.
- No spill dir configured → today's behavior stands (the full
  rendering returns; better everything than a pointer to nowhere).
- Errors and notice-only results never spill.

## Acceptance Criteria

- A search producing > 30 KiB of matches returns hint-first with
  the full list on disk; one within the budget is byte-identical to
  today. Existing truncation notices (entry budget, match cap)
  still appear in the preview.

## Milestone

13 — Guard rails
