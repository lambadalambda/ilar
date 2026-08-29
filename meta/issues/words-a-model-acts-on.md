# Words a model acts on

## Summary

Model-facing text that misleads, gathered: (1) the concurrency
refusal says "Do not retry. … then try again" in one breath
(subagent.rs:766-770); (2) service start echoes "pid group
Some(1234)" — a Debug format on the wire (service.rs:224-227); (3)
`describe()` reports a daemonized service "stopped (exit 0)" while
its group runs — inviting a pointless restart (service.rs:440-452);
(4) the id-less tool-call decode error implies stream corruption
instead of naming the server's omission (chat.rs:531-534) — or
better, synthesize an id from the index for `[models.*]` endpoints.

Size: S each. Source: sweep 2026-08-29, subagent + tools + providers.
