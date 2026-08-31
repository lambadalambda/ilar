# Current codebase review

## Summary

Review the current `ilar` and `ilar-tui` codebase for correctness risks, architectural friction, quick wins, and development or release blockers.

## Requirements

- Cover both workspace crates and their tests.
- Verify findings against the current source and cite concrete locations.
- Separate confirmed problems from judgment calls and already-tracked work.
- Avoid style-only findings and speculative rewrites.

## Acceptance Criteria

- A prioritized review report identifies blockers, correctness risks, architectural concerns, and quick wins.
- New actionable findings are added to the issue tracker or explicitly identified as already tracked.
- Baseline build, test, formatting, and lint status is reported where feasible.

## Notes

- This is a review task; code changes are out of scope unless separately requested.

## Outcome

The review covered roughly 100k lines of Rust across both crates with parallel reviewers for core orchestration, persistence, tools, providers/configuration, and both TUI surfaces, followed by an independent validation pass over the accepted shortlist.

### Priorities

- **Blockers:** image ingestion can allocate from an unbounded dropped file or decoded PNG dimensions before its cap; the checked-in tree fails the documented formatting gate and Linux all-target clippy gate.
- **Significant correctness:** post-`TurnStarted` compaction/provider preflight errors can omit terminal `TurnDone`; ignored project routing tables can still reject startup; damaged out-of-range rewind markers silently fail open.
- **Responsiveness/resource risks:** obsolete TUI replay workers continue after retargeting; `history` performs a blocking full replay and can return an unbounded speaker listing; skill discovery repeatedly reads every full body synchronously and has no body cap.
- **Quick wins:** a delivering child focus starts with a permanently running footer; temporary Git repositories inherit global commit signing; built-in provider URLs are concatenated without startup validation or normalization.

Each accepted new finding is filed under **Review follow-ups** in `meta/issues.md`. Existing structural concerns remain tracked by `The big files find their seams`, `One fold for live and restored`, `Live rows rerender every frame`, and the parked serve correctness issues.

### Verification

- `cargo test --workspace --all-features` passed on `tenco.local`: 1,381 passed, 0 failed, 8 ignored across 33 suites.
- The ordinary local suite exposed five fixture failures caused solely by inherited Git commit signing; disabling signing made those tests pass and the remote hermetic run was green.
- `cargo fmt --all -- --check` failed with diffs in 17 files.
- Linux `cargo clippy --workspace --all-targets --all-features -- -D warnings` failed on two platform-dependent conversions in `atomic_file.rs`.

### Judgment calls

- Ordinary session appends call `write_all` and `flush`, not `sync_data`, before success. This is process-crash safe in the usual sense but not a power-loss durability guarantee. The docs' word “crash-safe” should be clarified before paying the throughput cost of per-event syncing.
- The default terminal architecture is otherwise coherent: provider-neutral streaming, explicit cancellation, bounded event channels, append-only recovery, writer leases, and barrier-scheduled tools all have unusually strong invariant tests. The external sandbox remains an intentional security boundary; the open kernel-sandbox issue is a deployment constraint, not an accidental omission.
