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

## Outcome (2026-09-03)

Formatted the workspace with the current stable rustfmt (1.9 / Rust
1.98) as its own `style:` commit — 17 files of rewrapped macro
arguments. Twelve clippy sites fixed on both platforms: the `mode_t`
widening in atomic_file.rs now goes through one `mode_bits` helper that
allows the cast lint where it is a no-op (Linux) and needs it (macOS);
`as_chunks` replaces constant `chunks_exact`; a `next_back`, two
off-by-one comparisons, a redundant closure, and four type aliases in
the TUI. `scripts/check.sh` is the gate (fmt check, clippy
all-targets/all-features with `-D warnings`, tests all-features) and
AGENTS.md names it and the toolchain. No CI exists to wire it into; the
script is what CI would run. Verified: exit 0 on tenco (Linux), clippy
clean on macOS.
