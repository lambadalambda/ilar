# Local tool-call security experiment

## Summary

Compare local security checkers on a small set of sanitized ilar session calls and hand-labeled dangerous and authorization-sensitive variants. This is an offline experiment, not a runtime security feature.

## Requirements

- Sample real session calls without committing private transcripts or credentials.
- Store a reusable labeled test set and a runner that never executes tested calls.
- Run at least one locally hosted checker, preferably TS-Guard, and record errors, false approvals, false blocks, and latency.
- Keep model-specific prompts separate from labels and include benign as well as unsafe calls.

## Acceptance Criteria

- The sanitized cases and expected decisions can be reviewed in the repository.
- Runner tests pass and a local model run produces inspectable per-case results.
- DEVLOG records results and limits without claiming a security guarantee.

## 4-bit follow-up

Compare local 4-bit MLX conversions on the unchanged 30 cases. Measure
ShieldAgent's first complete verdict instead of continuing its explanation.
Include an unquantized MLX ShieldAgent control to separate quantization from
engine and stopping changes. Record decision changes, timing and model sizes.

## Results

Completed the 30-case development set, local/HTTP runner and five focused tests.
TS-Guard ran locally with no parse errors. Its one unsafe approval and benign
false blocks show why it must not replace execution restrictions. Full comparison
and raw outputs are in `experiments/tool-guard/RESULTS.md` and `results/`.
No Rust runtime behavior changed; the Rust workspace suite was not run.

The 4-bit follow-up is complete with four MLX runs, eight focused tests and
independent review. ShieldAgent's matched verdict-only mean fell from 1.338 s
(FP16) to 1.186 s (4-bit), but two denials became unsafe approvals. TS-Guard's
4-bit MLX run averaged 7.236 s and escalated more benign cases. Raw results and
measurement limits are recorded with the first experiment.
