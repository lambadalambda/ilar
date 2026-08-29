# Serve steps out of the default build

## Summary

`ilar serve` is the largest single subsystem in the TUI crate — six
and a half thousand lines across `watch`/`view`/`drive`/`http`, plus a
Preact frontend — and the people using ilar today use the terminal.
The second health sweep found the cost of keeping it level: serve has
forked the delivery semantics (no retire, no salvage, adoption once
per process, three hand-rolled delivered-predicates, no watchdog), it
folds transcripts with its own copy of the fold, and every core
refactor now has to keep a second consumer honest.

The decision is not to delete it but to stand it down: put it behind a
Cargo feature that is off by default, so the agent can be got into
shape without a web view riding along on every change.

## Requirements

- A `serve` feature on `ilar-tui`, **off by default**, gating the
  `serve` module, the `Serve` subcommand and its args, and the
  serve-only dependencies (axum, getrandom). `base64` stays
  unconditional — the clipboard uses it.
- `crates/ilar-tui/tests/serve.rs` compiles and runs only under the
  feature.
- The default `ilar --help` does not advertise a subcommand that is
  not there.
- Docs say so plainly: README, `docs/serve.md`, `docs/sessions.md`
  note that serve is dormant and how to build it.
- The filed serve issues are marked as parked rather than deleted:
  they describe real defects that come back the day the feature does.

## Acceptance Criteria

- `cargo test` (default features) is green and builds no axum.
- `cargo test --features serve` is green — the code is stood down,
  not abandoned.
- `ilar --help` on a default build lists `login` and `exec` only.

## Notes

- Accepted cost of a feature that nothing builds by default: it rots.
  The bargain is explicit — when the flag is next flipped on, whatever
  broke gets fixed or the module gets deleted, and that is a decision
  made then, with the web frontend's fate, not now.
- Related: [[web-frontend]], [[serve-retires-what-it-cannot-route]],
  [[serve-joins-the-turns-it-started]], [[serve-folds-once-and-caches]],
  [[the-outbox-is-reread-while-serve-lives]],
  [[serve-kills-the-background-children]], [[one-delivery-engine]].

## Outcome

`serve = ["dep:axum", "dep:getrandom"]`, default features empty. The
module, the `Serve` subcommand, its args and the dispatch arm are all
`#[cfg(feature = "serve")]`; `tests/serve.rs` carries the inner
attribute. `base64` stayed unconditional — the clipboard writes it.
Default build: 426 TUI unit tests, no axum in the tree, and `--help`
offering `login` and `exec`. With the feature: 493 unit tests and the
24 wire tests, green. README, `docs/serve.md` and `docs/sessions.md`
say it plainly; the serve issues are marked *parked with serve* rather
than closed.
