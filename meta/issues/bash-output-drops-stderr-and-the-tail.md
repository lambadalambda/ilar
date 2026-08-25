# Bash output drops stderr and the tail

## Summary

The bash tool's `drain` (bash.rs:19-41) keeps only the *first*
100 KiB per stream; `render_output` (bash.rs:98-120) concatenates
stdout-then-stderr and truncates to the first 100 KiB again. A
failing build that prints ≥100 KiB to stdout yields a tool result
with zero stderr and no tail — exactly where the error message
lives. `service.rs:25-42` has a near-twin `drain` that correctly
keeps the tail; the two also duplicate the `sh -c` +
`process_group(0)` spawn setup and the `libc::killpg` SIGKILL
incantation.

## Requirements

- Retention becomes tail-biased, and stderr is preserved in the
  rendered result even when stdout fills the cap (e.g. per-stream
  tail caps before concatenation).
- Share one `drain` (and the process-group kill/spawn helpers)
  between bash.rs and service.rs so the policies cannot diverge
  again.

## Acceptance Criteria

- A test: a command emitting >100 KiB to stdout plus a final stderr
  line yields a result containing the stderr line and the stdout
  tail.

## Milestone

12 — Health sweep

## Outcome

A shared `tools/process.rs` now owns `Captured` (tail-biased
retention), `drain`, the `sh -c`/process-group spawn shape, and the
one `killpg` site; bash and service both use it. `render_output`
budgets the rendered cap per stream — stderr always keeps up to half
when it needs it, both streams tail-sliced on UTF-8 boundaries — and
reports actual raw totals. Pinned by
`bash_keeps_stderr_and_the_stdout_tail_when_stdout_fills_the_cap`
plus process.rs unit tests. (ce41a84)
