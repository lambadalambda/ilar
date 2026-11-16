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

- One process-launch point already exists:
  `tools/process.rs::shell_command` builds every child for bash and
  services. The experimental macOS backend uses the system sandbox helper;
  the initial direct-`pre_exec` sketch is superseded (see results below).
- Backends: macOS Seatbelt; planned Linux Landlock (`landlock` crate,
  unprivileged, stackable; network TCP on ≥6.7) + seccomp for
  syscall classes. Refuse enabled confinement on unsupported backends.
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
- Retry after a grant only if execution did not start, or the operation
  is known to be safe to repeat. Otherwise show partial results and ask
  before restarting; a denial can occur after a publish or migration.
- Three modes: off (yolo), default (the above — the product),
  strict (no baked allowlist, read shielding, every escalation
  asks).

## Reference baseline

The user's real, evolved safehouse invocation (2026-08-25) — the
policy to match or beat with fewer and narrower grants:

`safe-agent --browser --ssh --ro ~/Downloads/ --enable xcode
--keychain --rw ~/Documents --rw ~/repos --ro ~/ --podman
--rw ~/Library/ --rw ~/.local/state/ --rw ~/.config/ilar
--agent-browser ilar`

Readings: (a) `--rw ~/Library/` is a forced over-grant — macOS
toolchains write there (DerivedData, caches), and static
granularity made all-of-Library the only workable scope; observe
mode should identify the actual subpaths so the rest of Library —
where the "whoops" damage lives — stays protected. (b) the
`~/.local/state` and `~/.config/ilar` grants exist only because an
external wrapper covers the whole tree; under asymmetric
confinement children never need them — they disappear. (c) half
the flags are capabilities, not paths (ssh, keychain, xcode,
podman, browser) — these become named toolchain profiles that
project inference enables (an .xcodeproj turns on the xcode
profile unprompted). Shape of the evolved default: RO-broad home,
RW islands for work areas, capability profiles, nothing granted to
children that only the host process needs.

## Requirements

- A probing design pass first (like the image feature): verify
  Seatbelt profiles against real cargo/git/npm workloads in this
  repo before committing to defaults; then slice.

## Experimental implementation (2026-11-15)

The first implementation is opt-in through user configuration or a CLI
option. Existing launches must not change behavior. Start with macOS
process confinement and test real build workloads before selecting
production defaults. Unsupported backends must refuse an enabled sandbox,
not silently run without it.

- Keep broad reads and narrow write paths. Separate cache writes from
  configuration writes and executable installation. Never grant all of
  `.local`, `.config`, or `Library` just because one tool needs a subpath.
- Use named toolchain profiles with explicit cache paths. Project files
  can suggest profiles, but cannot authorize powerful capabilities or
  modify saved grants. User-level configuration is the trusted source.
- Let calls request extra write paths, network access, or execution
  without ilar's sandbox, with a reason. Approval happens before launch.
  Missing approval support means refusal. A bypass is for one call and
  does not remove an inherited external sandbox or secret/root checks.
- Observe mode is advisory: accesses produce proposed grants, never
  automatic approval. Make its enforcement limits explicit. Accesses can
  come from malicious project scripts; they are not proof of need.
  Reliable access recording needs a separate platform-specific design.
- Saved project grants belong outside repository-controlled files.
  Coalesce requests; avoid repeated low-value questions. Do not infer a
  sandbox denial from a generic permission error or retry partial work.
- Process confinement alone does not constrain host-side file tools.
  Document that boundary until direct tools enforce the same path policy.
- Existing services need restart to change confinement. A grant does not
  widen an already-running process. Container sockets and other brokers
  can perform work outside the confined child and need separate treatment.

Record example-project results and remaining gaps before widening this
experiment. Keep this issue open until the full grant and policy design
is implemented and tested.

### Prototype results

Implemented user-only `[sandbox]`, `--experimental-sandbox`, broad-read/narrow-write
macOS policy, explicit Cargo/pip/npm cache profiles, and root TUI once-only
extension/bypass approval. All process-tool launch paths, including background
bash and sudo's probe, share the policy. Direct file tools are not covered.

The prototype uses system `sandbox-exec`, not `sandbox_init` in `pre_exec`:
policy compilation after fork can allocate or lock. Approval details are
scrollable. Experimental sudo does not retry launched commands based on their
output. No saved grants, observe mode, Linux backend, or child/gateway approval
route is implemented yet.

Four installed-toolchain projects passed both without and with the sandbox.
The initial runner blocked activation with EPERM; after the user removed the
outer sandbox, all four backend checks and all seven integration tests passed.
Host checks confirmed narrow write grants, denied sibling/symlink writes,
preserved partial work, and network denied unless granted. Named cache profiles
and network-dependent package installation still need workload tests.
See [experiment commands and results](../../experiments/sandbox/README.md).

## Narrow read defaults (requested 2026-11-15)

Within the opt-in sandbox, deny file content reads outside the workspace,
explicit read/write grants, system runtime paths, and narrow toolchain/cache
profiles. Do not grant the whole home, `.config`, `.local`, or `Library` tree.
Allow metadata probes needed for command lookup without allowing directory
contents or file data. Add `read_paths` to trusted user configuration and to
once-only call approvals, with read and write scopes shown separately.

Check denied unrelated-home reads, outward symlinks, approved read-only access,
and Cargo/Python/npm/Git workloads on the host. Do not silently widen the policy
to fix a failed build. Keep direct host-side file tools outside the claimed
process boundary; no automatic retries or denial-generated prompts are added.

Implemented `read_paths` in user config and once-only approval requests. Default
reads cover workspace, selected macOS runtime/developer files, selected Homebrew
installation trees, and Rust/mise installation paths. Cache profiles grant
read/write cache access; ordinary write grants do not add reads. Metadata probes
remain global, and dyld requires literal root-directory reads. Redirected default
user toolchain paths are omitted; explicit target grants can enable them.

Host tests confirm denied unrelated/symlink reads and directory listing,
read-only grants, write-only grants, and once-only read approval. All four
workloads pass with explicit Cargo/Git config reads; the Cargo probe uses the
system linker instead of a host mise shim. `xcrun` cache and optional Git-ignore
warnings remain. No whole-home or whole-config grant was added. See the
experiment notes for details and remaining limits.

## Session approvals (requested 2026-11-15)

Add an explicit "Allow for this session" choice for read paths, write paths,
and network. Reuse approved scopes in later process-tool calls of that live
session, without a new question. Keep read and write grants separate. Do not
share grants with another session, child agent, or restarted runtime. Store
nothing on disk. Execution without ilar's sandbox remains a once-only choice,
including requests that mix a bypass with path/network access.

Canonicalize requested paths before approval and retain those target paths,
not symlink aliases that could later redirect a saved grant. Keep explicit
once/deny choices and existing unsupported-client refusal. Check repeated
requests, calls without an extension argument, session isolation, and bypass
refusal for the session scope.

Implemented the three approval choices for root TUI process calls. Session
read/write/network grants use a clone-shared, session-keyed in-memory store;
children and fresh runtimes start empty. Calls without extension arguments and
repeated covered requests reuse grants. Bypass requests never offer session
approval. Retained canonical paths are discarded if resolution changes or fails.
Focused tests cover reuse, isolation, separate read/write scopes, bypass refusal,
and canonical-target replacement. A host test confirms later bash reads are
denied after once-only approval and allowed after session approval.

## Approval review follow-up

Astra's review found two small issues: configured aliases could cause repeated
approval requests, and unescaped filenames could make approval text ambiguous.
Compare configured read/write roots in canonical form, without combining their
scopes. Quote and escape paths and caller text, and separate caller text from
host-generated access fields. Keep implicit workspace/profile coverage and
filesystem race limits outside this follow-up.

## Direct file tools (requested 2026-11-16)

Confine `read`, `write`, `edit`, `glob`, and `grep` with the same policy and
once/session approvals as process tools. Use a short-lived sandboxed worker;
path checks in the privileged parent do not close symlink and replacement races.
Keep provider/config/session access in the parent. Transfer file-read digests
and image bytes through IPC, without reopening target files in the parent.
Keep workspace leases until worker exit, preserve edit freshness and cancellation,
and refuse calls if the worker or sandbox cannot start. Sandbox-off behavior is
unchanged. The worker is an installed sibling binary, never resolved through PATH.

Implemented with shared file-tool bodies and dedicated IPC. MacOS checks cover
workspace operations, stale edits, outside/symlink denial, separate read/write
grants, and once/session sharing with shell calls. Approval text includes the
proposed arguments. Atomic replacement needs read/write directory grants;
existing exact-file write requests are refused before approval rather than
silently widened. Worker cancellation retains its write lease through reaping.

## Optional sandbox arguments (2026-11-16)

The latest imgsearch transcript filled every sandbox field, then escalated from
an empty request to `/` after validation errors. OpenAI Responses can normalize
omitted `strict` into strict schemas. Explicitly send `strict: false` for our
optional tool arguments. Treat inactive sandbox objects as existing-policy calls,
recognize canonical workspace read/write coverage, and explain how to omit an
unneeded request in validation errors. Keep actual extensions and bypasses gated.

## Milestone

13 — Guard rails
