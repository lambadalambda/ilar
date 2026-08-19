# Service tool: managed long-running processes

## Summary

Agents cannot run servers: foreground bash kills the process group on
completion (by design), background bash is completion-oriented with a
timeout, and the setsid escape hatch leaks orphans. Add a first-class
`service` tool with tracked lifecycles.

## Requirements

- One `service` tool with actions: `start` (name + command), `status`
  (one or all: pid, running/exit status, uptime, command), `logs` (last
  N lines of combined stdout/stderr, bounded buffer), `stop` (kill the
  process group, report exit).
- Services are owned by a per-session ServiceManager: dropped (app exit
  or session switch) ⇒ every service's process group is killed. No
  orphans.
- Duplicate `start` on a running name errors; restarting a dead name
  replaces it.
- Subagents share the root session's manager (same registry wiring as
  todos/subagents).
- Pending manager shows "services: N running" with confirmed stop-all.

## Acceptance Criteria

- Tests: start/status/logs/stop round trip against a real process;
  duplicate-name error; manager drop kills the child (PID verified);
  exited services report their status.
