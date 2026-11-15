# Experimental process sandbox

Status: off by default; macOS backend only. See
[configuration](../../docs/configuration.md#experimental-process-sandbox) and
[design issue](../../meta/issues/kernel-sandbox-for-tool-processes.md).

## Scope

The sandbox applies to bash (foreground and background), service start, and
sudo, including its authentication probe. It does not constrain ilar itself,
direct file tools, Git helpers, provider requests, or every other subprocess.
Reads remain broad. This is not a sandbox for hostile code.

Enable it with `ilar --experimental-sandbox`,
`ilar exec --experimental-sandbox ...`, or user-only `[sandbox] enabled = true`.
The default policy allows workspace writes, uses that workspace for temporary
files, and denies network. Explicit profiles grant Cargo, pip, or npm caches;
explicit absolute write paths grant their subtrees. Cache parent directories
must exist. Network grants permit all network access.

Interactive root TUI calls can request once-only exceptions:

```json
{
  "command": "build-command",
  "sandbox": {
    "write_paths": ["/absolute/path/to/build-cache"],
    "reason": "The build writes its cache here."
  }
}
```

`network: true` requests network access. `unsandboxed: true` requests execution
without ilar's sandbox. Both need user approval. Exceptions are not saved and
do not remove outer restrictions or secret/root checks. Children, exec, and the
gateway refuse exception requests because they have no approval route yet.

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

## Results, 2026-11-15

On the development Mac, the real Seatbelt compiler accepted generated policies,
including escaped paths. Unit/config/approval tests passed. The installed-toolchain
baseline passed all four workloads:

| Workload | Without ilar sandbox | With ilar sandbox |
| --- | --- | --- |
| Cargo offline build and run, no dependencies | Pass | Activation blocked |
| Python byte compilation and run | Pass | Activation blocked |
| npm offline local dependency install, explicit cache path | Pass | Activation blocked |
| Git init, add, staged diff | Pass | Activation blocked |

All four sandboxed attempts exited 71 with
`sandbox-exec: sandbox_apply: Operation not permitted`. The requested commands
did not start. This runner cannot activate Seatbelt; removing ilar's added
sandbox cannot remove inherited external restrictions. These results establish
failure without fallback, not working write/network enforcement or useful
cache defaults. Enforcement tests remain explicit host tests, not silently
skipped successes.

## Checks

Focused policy, configuration, approval, and TUI question tests passed. Workspace
Clippy passed with all targets/features and warnings denied. The workspace test
run had one failure in the existing serve transcript-pagination test; its
focused all-features rerun passed. Doc tests passed. Strict test-first was not
completed for the cross-module prototype; regression tests were added and run.

## Not implemented

- Reliable observe/training mode. Local unified-log access is blocked;
  `eslogger` needs elevated privileges. No access trace was collected, and no
  policy was learned from command errors. Future observations must propose
  grants, never approve them automatically.
- Saved per-project/session grants and automatic toolchain-profile inference.
- Strict read/secret-file confinement, direct file-tool policy, host allowlists,
  and Linux confinement.
- Gateway/child approval routing and a separate private temporary directory.

Broad reads, IPC/brokers, inherited descriptors, cache poisoning, pathname
replacement/hard links, and descendants that leave the process group remain
limits. Do not treat a service's initial "started" result as proof of activation;
check status/logs. Do not enable this as a production default until successful
host enforcement and workload experiments establish its behavior.
