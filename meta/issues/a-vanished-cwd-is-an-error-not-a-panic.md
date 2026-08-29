# A vanished cwd is an error, not a panic

## Summary

`WorkspaceLocation::shared` panics on an unresolvable cwd
(tools/mod.rs:62-64) and is called from `SubagentSpawner::new` and
`ToolContext::root`: launching or resuming with a deleted cwd
aborts the whole process. `try_shared` already exists.

## Fix

Runtime constructors use `try_shared` and propagate.

Size: S. Source: sweep 2026-08-29, store/workspace.
