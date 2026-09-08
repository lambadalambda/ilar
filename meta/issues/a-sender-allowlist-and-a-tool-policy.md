# A sender allowlist and a tool policy

## Summary

ilar's stance is that the sandbox is the permission system, which is
right for a terminal and wrong for a box anyone can message. Before a
channel is exposed: who may talk, and what the model may run on their
behalf.

## Requirements

- Per channel: allowed sender ids; unknown senders get no turn and one
  fixed reply at most.
- Tool policy per agent or per chat: allow/deny lists over tool names,
  and a safe mode that drops bash, write, edit and service.
- Policy is enforced by the registry the gateway hands the runtime, not
  by the prompt.

## Acceptance Criteria

- An unlisted sender never reaches the loop; a denied tool is absent
  from the model's tool list, not refused at call time.
