# Service commands are redacted too

## Summary

`bash` command strings pass through `redact_command` twice — in the
one-line summary (`agent/turn.rs:780`) and in the expanded argument
detail (`turn.rs:854`, gated on `name == "bash" && key ==
"command"`). `service` takes a `command` string and runs it in a
shell exactly the same way (`tools/service.rs`), and matches
neither branch, so it falls through to the generic path. That path
only inspects key *names* (`sensitive_key`), so a secret in the
command *value* survives verbatim into the transcript, the
persisted session event, and the `ilar serve` JSON.

The identical text is redacted when it rides `bash` and published
when it rides `service`.

## Requirements

- Redaction follows the shell, not the tool name: both sites treat
  every tool that runs a `command` string in a shell alike. One
  list or one predicate, not two literals — the two branches
  already drifted once.
- Test with a token-bearing command through `service`, asserting
  the summary, the argument detail and the persisted event.
- Check whether any other tool takes a free-form command value
  (`process`, background jobs) and cover it in the same predicate.

## Milestone

13 — Guard rails

## Outcome

Landed as 1fab64e: both display sites key on
`shell_command_argument` — one predicate over command-shaped keys
(command, cmd, commandline, shellcommand) instead of two `bash`
literals — so `service` and anything else that runs a shell string
redacts wherever `bash` does. Pinned by
`a_service_command_is_redacted_everywhere_a_bash_one_is`, which
asserts the summary, the argument detail, the persisted event and
the live scratch. Verified closed during the 2026-08-29 backlog
sweep. What this issue never covered — the command echoed back in
a tool *result* body, and credentials embedded in URLs — is filed
as redaction-covers-what-tools-echo.
