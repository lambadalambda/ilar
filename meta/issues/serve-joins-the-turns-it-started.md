# Serve joins the turns it started

## Summary

Web-driven turns are spawned and their handles dropped
(drive.rs:582); shutdown cancels tokens and then tears down the
spawner and services while such a turn may still be unwinding —
the process can exit mid-write, tearing the final log events the
TUI always lands.

## Fix

Keep the JoinHandle (the registry entry is the natural home) and
await all driven turns under `SHUTDOWN_GRACE` before
spawner/services teardown.

Size: M. Source: sweep 2026-08-29, serve.
