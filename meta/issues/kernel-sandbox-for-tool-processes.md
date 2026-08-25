# Kernel sandbox for tool processes

## Summary

ilar currently relies on external sandboxing (the user wraps it in
safehouse). Build kernel-level confinement into ilar for spawned
tool processes instead: ilar itself runs normally (provider
network, state dir), but every bash/service child is confined. The
experience with external wrapping: read rights end up broad anyway
because static policies with no interactivity force it — and that's
acceptable, because the dominant accident class is writes ("whoops,
deleted your Library folder"). Write-confinement plus
network-deny-by-default is the 80% win; interactivity is what makes
anything tighter viable.

## Design sketch (from discussion, 2026-08-25)

- One enforcement chokepoint already exists:
  `tools/process.rs::shell_command` builds every child for bash and
  services. Confine there via `pre_exec` — no wrapper binary,
  inherited by the process group ilar already manages.
- Backends: macOS Seatbelt (`sandbox_init`, generated profile —
  what Codex CLI/Chrome ship on); Linux Landlock (`landlock` crate,
  unprivileged, stackable; network TCP on ≥6.7) + seccomp for
  syscall classes. Old kernels: warn and run.
- Default policy: read broad, write confined to cwd + /tmp +
  session-granted paths + known caches (~/.cargo, ~/.rustup, …);
  network deny by default.
- Interactive grants are the differentiator over external wrapping:
  a denial surfaces as a distinguishable tool error AND an
  interactive question ("command wants network — allow once /
  session / project / deny"), grants persist per-project so
  questions taper. Denials land in the session log (auditable via
  history).
- Asymmetric by construction: ilar keeps provider access while
  children have none. Stacks under an external sandbox (defense in
  depth, keep safehouse supported/documented).
- Staging: (0) observe mode — log-only, no enforcement, to measure
  what real sessions touch and tune defaults empirically before
  they ever block anything; (1) write-confinement + network-deny +
  grant flow, Seatbelt; (2) Landlock parity; (3) opt-in strict
  read/secrets shielding. README's "no sandbox" warning retires at
  stage 1.

## Defaults & UX principles (binding)

The product is the middle ground between "annoyed every other
turn" and yolo. A question must be an event, not a rhythm — every
low-signal prompt trains reflexive allowing and devalues the one
that matters. Mechanisms, all required:

- Capability-shaped defaults: workspace/tmp/toolchain-cache writes
  silent; network to a curated infra allowlist (package registries,
  git hosts, localhost) silent. Arbitrary external hosts ask.
  Default mode targets accidents and casual injection; determined
  exfil (e.g. via github.com) is strict mode's problem.
- Project-inferred grants: lockfiles/toolchains/git remotes
  generate per-project policy (Cargo.toml → crates.io + ~/.cargo,
  package.json → npm, remotes → their hosts). This is the
  advantage over static external wrappers — use it.
- "Allow for this project" is the default answer scope; persisted,
  so each unique question fires once per project ever. Coalesce
  multiple denials from one command into one question.
- Transparent retry: after a grant, ilar re-runs the denied call
  itself — the model never sees the denial in the common case, no
  wasted turns.
- Three modes: off (yolo), default (the above — the product),
  strict (no baked allowlist, read shielding, every escalation
  asks).

## Requirements

- A probing design pass first (like the image feature): verify
  Seatbelt profiles against real cargo/git/npm workloads in this
  repo before committing to defaults; then slice.

## Milestone

13 — Guard rails
