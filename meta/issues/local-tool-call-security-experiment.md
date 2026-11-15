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
