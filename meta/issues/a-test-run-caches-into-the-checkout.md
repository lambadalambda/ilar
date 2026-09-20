# A test run caches into the checkout

## Summary

`cargo test -p ilar` leaves `crates/ilar/.local/state/ilar/endpoints/lemon.json`
behind, untracked, in the working tree.

Two things combine:

- `Loader::resolve_dirs` falls back to `./.local/state/ilar` when
  `HOME` is unset (`config/toml.rs:719-726`). It flags that as
  `Dirs::homeless` and `require_home` exists to refuse it, but
  `resolve()` never calls it — so a library consumer, or a test,
  scatters state into whatever directory it is standing in. 54 of the
  56 `Loader::no_env()` call sites in `crates/ilar/tests/config.rs`
  resolve without a state directory, so they all land there.
- `base_urls_are_canonical_and_refused_by_field`
  (`tests/config.rs:1042`) names `http://127.0.0.1:13305/api/v1/` — the
  port Lemonade listens on. On a machine running Lemonade the test
  makes a real request to the user's model server, succeeds, and
  `endpoints::discover` caches the listing. That is why the file
  appears on tenco and not on the Mac.

## Requirements

- Nothing is written to a state directory that was guessed rather than
  chosen. The listing is still used; only the remembering is refused,
  and a warning says why.
- No test reaches a service on a well-known local port. A test that
  wants a dead endpoint uses `127.0.0.1:9`, as
  `a_dead_server_falls_back_to_the_cache_and_says_so` already does.

## Acceptance Criteria

- A test asserts that resolving a config with a live endpoint and no
  state directory writes no cache under the working directory.
- `cargo test` on a machine running Lemonade leaves a clean tree.

## Notes

- Noticed 2026-09-20 while restoring tenco's checkout after a gate run.
- Deliberately not solved by adding `.local` to `.gitignore`: that
  hides the next occurrence rather than stopping it.

## Outcome (2026-09-20)

Both halves. `endpoints::discover` takes `Option<&Path>`, `None`
meaning the state directory was guessed: the listing is used, nothing
is written, and a warning says why rather than the refusal being
silent. `Config::load` passes `Dirs::homeless` through to decide it.
The fixture that named Lemonade's port now names the dead one.

Pinned by `a_guessed_state_directory_does_not_collect_a_cache`, which
asserts on the path under the working directory, and by
`a_guessed_state_directory_is_never_written_to` on the function.
Verified by restoring the old behaviour on tenco and watching both the
test fail and the file reappear; the gate then ran green there and left
a clean tree, which it had not been doing.

Left alone: the 54 call sites still resolve state to the working
directory. Nothing writes there now, and rewriting them all is churn —
but a future writer under the state directory would revive this, so the
real guard is that `resolve()` still does not call `require_home`.
