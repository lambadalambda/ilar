# A tool summary redacts only its generic path

## Summary

`summarize_tool_input` (`agent/turn.rs:1009`) is treated by four
consumers as a redacted projection. It is one only in part.

`redacted_argument` — sensitive keys, shell commands, URL credentials
— runs in `generic_summary` (`turn.rs:1165-1178`), and `redact_command`
runs in the `bash` and `service` arms. Every other named arm returns
its free text as written: `task_message`'s message, `question`'s
prompt, `grep`'s pattern, the paths of `read`/`write`/`edit`, `glob`'s
pattern.

So `· task_message t1 · fetch https://user:pw@host/x` reaches a
progress row with the credentials in it. The consumers are
`session_view`, `serve`, the gateway's status board and — new since
2026-09-20 — `ilar exec`'s stderr rows, which are redirected to files
far more often than a terminal transcript is.

## Requirements

- The named arms' free-text values go through `redacted_argument` too,
  so the function's contract is the one its callers already assume.
- The comment in `ilar-tui/src/exec.rs` that says the summary is
  redacted becomes true, rather than hedged as it is now.

## Acceptance Criteria

- A test pins a URL credential and a sensitive-looking value surviving
  neither a `task_message` nor a `grep` summary.
- The four consumers are unchanged: this is fixed at the source.

## Notes

- Pre-existing; found by the review of the `small-three` branch,
  2026-09-20, which added the fourth consumer. Not fixed there because
  it changes a function four surfaces share.

## Outcome (2026-09-20)

Fixed at the source, as asked: the `string` closure every arm uses now
goes through `redacted_argument`, and so do the two places that read
free text without it — the `question` arm's first prompt and
`summarize_task_input`'s description. The `bash` and `service` arms'
explicit `redact_command` calls went with it; they were doing by hand
what the policy already does, which is how the two had drifted apart
before.

Pinned by `every_arm_redacts_its_free_text_not_only_the_generic_one`,
which walks eight tools and asserts a credentialed URL survives none of
them while the rest of the value survives all of them. Verified by
restoring the old closure on tenco and watching three tests fail.

The four consumers are unchanged, and `ilar-tui/src/exec.rs` no longer
hedges about what the summary guarantees.
