# Experimental process sandbox

Status: off by default; macOS backend only. See
[configuration](../../docs/configuration.md#experimental-process-sandbox) and
[design issue](../../meta/issues/kernel-sandbox-for-tool-processes.md).

## Scope

The sandbox applies to bash (foreground and background), service start,
sudo (including its authentication probe), and direct file tools through
`ilar-file-worker`. It does not constrain ilar itself, Git helpers, provider
requests, or every other subprocess.
Content reads are restricted to workspace, runtime/toolchain defaults, selected
cache profiles, and explicit read paths. See configuration for the default path
groups. This is not a sandbox for hostile code.

Enable it with `ilar --experimental-sandbox`,
`ilar exec --experimental-sandbox ...`, or user-only `[sandbox] enabled = true`.
The default policy allows workspace writes, puts temporary files and tool caches
under workspace `.ilar-cache`, and denies network. Explicit profiles grant
read/write Cargo, pip, or
npm caches. Explicit absolute `read_paths` and `write_paths` grant separate
read and write access to their subtrees. Cache parent directories must exist.
Network grants permit all network access. Metadata probes remain globally
allowed; root-directory enumeration is allowed for macOS startup. Neither
exception grants content reads across the home directory.

Interactive root TUI calls can request exceptions and choose once or live-session
approval for paths/network:

```json
{
  "command": "build-command",
  "sandbox": {
    "write_paths": ["/absolute/path/to/build-cache"],
    "reason": "The build writes its cache here."
  }
}
```

`read_paths` requests read-only access, with no write grant. `network: true`
requests network access. `unsandboxed: true` requests execution without ilar's
sandbox. New extensions need user approval. A denied operation does not open
a prompt automatically: the agent must request an extension for a later call.
Session approvals are reused by later process calls, including calls without an
extension argument. They remain in memory only, are not shared with children,
and end when the runtime closes; restarting or resuming does not restore them.
Unsandboxed execution is once-only, even when combined with paths or network.
Approved paths use the canonical targets shown in the prompt. Retained targets
are discarded if they later resolve elsewhere or cannot be resolved. Exceptions
do not remove outer restrictions or secret/root checks. Children, exec, and the
gateway refuse new exception requests because they have no approval route yet.

The backend uses the system `sandbox-exec` helper, not `sandbox_init` after
fork: the latter can allocate or lock in a multithreaded parent's pre-exec
hook. Existing process-group capture and cleanup apply to the helper. There
is no unsandboxed fallback. A helper activation error is not evidence that a
specific file or network operation was denied.

## Reproduce

The project experiment creates disposable Cargo, Python, npm/local-dependency,
and Git projects. No provider calls or downloaded dependencies are needed.
Run from the workspace root on macOS with Cargo, Python 3, npm, and Git in PATH.

```sh
# Configuration, approvals, and default behavior; no activation required.
cargo test -p ilar --lib sandbox
cargo test -p ilar --test config sandbox
cargo test -p ilar --test sandbox

# Installed-toolchain baseline, without ilar's sandbox.
cargo test -p ilar --test sandbox example_project_baseline -- --ignored --nocapture

# Actual confinement: requires a host that permits Seatbelt activation.
cargo test -p ilar --lib sandbox::tests::macos_ -- --include-ignored --nocapture
cargo test -p ilar --test sandbox -- --include-ignored --nocapture
```

The confinement tests check workspace/granted writes, denied sibling writes,
outward symlinks, partial execution before a denial, and localhost network.
An approval test uses fake sudo (not root) to verify that a command's diagnostic
text cannot cause a second execution under the same sandbox exception.

## Initial write/network results, 2026-11-15

On the development Mac, the real Seatbelt compiler accepted generated policies,
including escaped paths. The initial runner blocked activation with EPERM. After
the user removed that outer sandbox, all four backend checks and all seven
integration tests passed, including the explicit host tests. No code change was
needed for the rerun.

| Workload | Without ilar sandbox | With ilar sandbox |
| --- | --- | --- |
| Cargo offline build and run, no dependencies | Pass | Pass |
| Python byte compilation and run | Pass | Pass |
| npm offline local dependency install, explicit cache path | Pass | Pass |
| Git init, add, staged diff | Pass | Pass |

The host tests confirmed allowed workspace/explicit-path writes, denied sibling
writes, denied writes through outward symlinks, preserved partial work before a
denial, and localhost TCP denied by default but permitted with a network grant.
These are focused checks, not proof of complete isolation. The npm project used
an explicit cache grant; these workloads do not validate all named cache profiles
or network-dependent package installation.

## Narrow read follow-up, 2026-11-15

The enabled sandbox now denies file-content reads except for workspace,
selected system/runtime and installed-toolchain paths, cache profiles, and
explicit read grants. Unrelated file reads, directory listing, and outward
read symlinks are denied. Explicit reads grant no writes; explicit writes grant
no reads. A once-approved read works only for that call. These behaviors have
host tests; ordinary workspace reads are checked too, so a failure to start the
shell cannot count as successful confinement.

The four workloads pass with the tighter policy and these explicit grants:

- Cargo: read `~/.cargo/config.toml` and `~/.cargo/config`. Use `/usr/bin/cc`
  as linker to avoid a host mise shim that needs its own user config.
- Python: no extra grants for the installed mise interpreter.
- npm: read and write the fixture's external cache directory.
- Git: read `~/.gitconfig` and `~/.config/git/config`.

Cargo and Git initially failed on these ungranted config reads, as intended.
Those files are not default grants because they can contain credentials.
Apple `xcrun` still reports denied writes to its hard-coded per-user cache;
Git reports a denied optional global-ignore read. The workloads complete, and
no whole-temporary-directory or whole-config-directory grant was added to
silence those warnings. This is not complete toolchain or cache-profile coverage.

The macOS dynamic loader also needs to read the root directory. A literal `/`
grant lets it start; a recursive root grant is not used. The default still
exposes filesystem metadata and installed-tool data, not only project files.

## Checks

The read follow-up passed 19 sandbox library checks, two configuration checks,
and all 10 integration checks, including explicit host tests. Workspace Clippy
passed with all targets/features and warnings denied. The full all-features
workspace test suite and doc tests passed. The initial prototype did not use
strict test-first throughout; the read follow-up started with failing read-denial
and redirected-toolchain regression tests.

## Session approval results, 2026-11-15

Session approval and canonical-target replacement regression tests pass. The
host approval test checks later bash reads after both once-only and session
approval. All 10 sandbox integration checks, including four example workloads,
pass. Workspace Clippy passes with all targets/features and warnings denied;
the full all-features workspace suite and doc tests pass. Formatting checks
pass for the changed Rust files. An independent review found no remaining
blockers after retained canonical targets were revalidated on reuse.

## Coding-task experiment, 2026-11-16

Ran the imgsearch mixed-random-library task through `ilar exec` with the sandbox
enabled, in a separate worktree. Copied installed Node dependencies into that
worktree before the run; no package downloads or permission exceptions were
allowed. The agent implemented the feature without changing the user's active
checkout. Its feature patch is experimental and is not merged by this work.

Baseline: 91 frontend tests, Svelte checks, and the production build with asset
validation passed. Go tests could not start because `TMPDIR` was the repo root:
Go deliberately ignores a `go.mod` at the system temporary root. Mise tried to
read global configuration and write its shared trust records. Git tried to read
`.gitconfig`, and Apple's tool launcher tried to write `xcrun_db` in the user's
general temporary directory. Loading the web skill worked, but reading its
`~/.agents/skills/modern-web-guidance/INDEX.md` was denied. Earlier imgsearch
transcripts also show denied reads of `~/go/pkg/mod` download metadata.

These failures call for readable dependency/skill data and worktree-local
scratch space, not a whole-home read grant or shared-cache writes. Private user
configuration, arbitrary outside writes, network, and sandbox bypass remain
approval decisions.

### Final defaults and retest

Temporary files, Go build data, npm/XDG caches, Clang modules, and mise state
now use `.ilar-cache` below the workspace. Directory creation uses open directory
descriptors and rejects symlinks instead of following them with parent rights.
The Go module cache is readable but not writable. Trusted user skill roots are
resolved once at session setup; broad targets and inner symlink escapes do not
get extra access. Shell and direct-file workers use the same policy and cache
environment. Mise retains inherited version precedence and project-aware shims.
Selected native Apple tools avoid the shared `xcrun` cache launcher.

A fresh full ilar run (`4d602c2d-d758-4c5c-9069-90f406686c2b`) in the isolated
clone passed all 91 frontend tests, Svelte checks, and production build/asset
validation. Go started correctly. It exposed one more missing read: Homebrew's
macOS SDK pkg-config data, used to find `libffi.pc`. Added only that read-only
subtree, not Homebrew's whole `Library`.

A final ilar verification passed linked skill INDEX reads, `pkg-config --cflags
vips`, `mise exec -- go test ./internal/library`, and `mise exec -- go test
./internal/db`, with no extra grants. Config-free Git status worked, with a
warning for the denied private global-ignore file. Direct `.gitconfig` reads
remained denied. The broad Go suite still fails tests that bind a localhost
port: network, including loopback, remains an approval decision. No installs,
network grants, or changes to the active imgsearch checkout were used.

The experimental feature changes remain in `../imgsearch-sandbox-retest` and
the earlier `../imgsearch-sandbox-experiment`; they are not part of ilar's patch.
The ilar all-features workspace suite passes (2,111 tests, 27 ignored), as do
workspace Clippy and changed-file formatting. Real macOS checks pass for Go/cgo,
mise tasks, pinned Go selection, write confinement, and denied loopback access.

Limited follow-ups: a driver's alternate `RuntimeOptions.user_dir` is not used
for frozen skill registration yet, and native compiler selection does not fully
preserve project-selected compiler shims. Neither needs wider default access.

## Not implemented

- Reliable observe/training mode. The initial runner blocked unified-log access;
  `eslogger` needs elevated privileges. No access trace was collected, and no
  policy was learned from command errors. Future observations must propose
  grants, never approve them automatically.
- Persistent per-project grants and automatic toolchain-profile inference.
- Sensitive-file exclusions within otherwise allowed roots, host allowlists,
  and Linux confinement.
- Gateway/child approval routing and an OS-private temporary directory outside
  the workspace.

Metadata disclosure, readable installed-tool data, IPC/brokers, inherited
descriptors, cache poisoning, pathname replacement/hard links, and descendants
that leave the process group remain limits. Do not treat a service's initial
"started" result as proof of activation; check status/logs. Keep this experiment
opt-in while the missing controls and wider workload coverage remain open.
