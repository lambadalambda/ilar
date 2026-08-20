# AGENTS.md — working on ilar itself

## Build & test

- `cargo build` / `cargo test` from the workspace root.
- Core crate (`crates/ilar`) is TDD'd: run tests before committing changes
  to loop/tools/providers. The TUI crate is tested too: unit tests live
  beside the module they cover, and render assertions go through
  ratatui's `TestBackend`.
- `cargo clippy --workspace` and `cargo fmt` should stay clean.

## Conventions

- Conventional commits, small topical commits.
- Architecture decisions and research findings go in DEVLOG.md.
- Roadmap lives in meta/issues.md (see skill: repo-issues). Write an issue
  before working on anything non-trivial.

## Architecture in one paragraph

`ilar` core: a `Provider` trait streams SSE events; the agent loop is an
async state machine over `tokio::sync::mpsc<LoopEvent>`; tools implement a
trait with `ToolKind::ReadOnly | Mutating` and the executor schedules
read-only tools concurrently behind the barrier model; sessions are
append-only JSONL; subagents are `JoinSet` tasks writing child sessions,
completing with a synthetic message into the parent loop (the pattern both
Claude Code and opencode converged on).
