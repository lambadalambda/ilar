# The quality gates are green

## Summary

The checked-in tree does not satisfy its documented formatting and lint gates. `cargo fmt --all -- --check` reports changes in 17 Rust files. Linux `cargo clippy --workspace --all-targets --all-features -- -D warnings` reports platform-dependent unnecessary conversions in `atomic_file.rs`.

## Requirements

- Format the workspace with the pinned or documented Rust toolchain.
- Make the Unix mode conversions lint-clean on both macOS and Linux without changing behavior.
- Pin or document the toolchain used by CI so formatting does not drift silently.
- Run the same all-target/all-feature checks in CI or the project quality gate.

## Acceptance Criteria

- `cargo fmt --all -- --check` passes from a clean checkout.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` passes on Linux and macOS.
- `cargo test --workspace --all-features` remains green.

## Notes

- Linux clippy locations: `crates/ilar/src/atomic_file.rs:94`, `170`.
- Reproduced locally and on `secunda.local` during the current codebase review.
