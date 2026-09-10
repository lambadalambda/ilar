# The message tool refuses nonsense

## Summary

A chat got eight text-only messages each claiming to carry a photo,
after a first send with the photo attached was rejected for a
`channel` of `deltachat:12` (a session key, doubled into
`deltachat:12:12`). The tool did reject the bad address, but with a
message that did not say what was wrong, and it sent everything
after that without question.

## Requirements

- A session key in `channel` is taken apart into channel and chat,
  not doubled; a channel name with a colon and a separate chat is an
  error that says what `channel` is.
- A chat that has not written is refused with the known chats named.
- A text that speaks of an attachment while `media` is empty is
  refused, with the fix stated.
- The description says what `channel` and `media` are.

## Acceptance Criteria

- Unit tests for each refusal and for the lenient address.
