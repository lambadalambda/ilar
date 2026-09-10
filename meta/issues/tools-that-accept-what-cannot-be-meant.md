# Tools that accept what cannot be meant

## Summary

The message tool's family of faults, elsewhere: `cron` ignores
unknown fields (a `channel`/`chat` pair schedules for the home chat),
refuses a bare chat id without saying the shape, accepts empty
names and prompts and a one-second interval, and says "never fires"
for a past `at`; `message` doubles a session key given as `chat`;
`bash` takes a seconds-shaped `timeout_ms` and an empty command;
`history` reads its fields leniently so a wrong type changes the mode.

## Requirements

- `cron`: unknown fields refused; a bare id means this channel; a
  refusal names the shape and the known chats; empty name or prompt
  refused; `every_secs` floored at 60; a past `at` says so with the
  current time; remove accepts a unique name; list shows the schedule.
- `message`: a session key in `chat` is taken apart.
- `bash`: `timeout_ms` under a second refused with the unit named;
  an empty command refused; the schema describes `command`.
- `history`: fields of the wrong type are a type error.

## Acceptance Criteria

- Tests for each refusal and for the lenient cron target.
