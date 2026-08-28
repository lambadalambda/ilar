# Images ride with steers and queued messages

## Summary

Attach an image, type a message while a turn is running, press
Enter: nothing is sent. `decide::submit` sees attachments with a
target of Steer or Queue and returns `PasteInput(text)` plus a
warning telling the user to wait for the turn to end. So the
message reappears in the input box and the images sit there — which
reads as two bugs at once ("images don't send" and "the message
doesn't go away") and is the same mistake as refusing a session
switch over a waiting stash: keeping something safe by holding the
person hostage.

Steering is exactly when a screenshot is most useful — "no, look at
this" — so the fix is to carry them, not to explain why we won't.

## Requirements

- The steer channel carries images alongside text, and the
  `UserMessage` a steer appends to the session carries them, so the
  model sees the image on its next step. Today `turn.rs` appends
  `images: Vec::new()` unconditionally.
- A queued message keeps its images until it is sent; sending
  restores them the way a fresh turn's attachments already work.
- The pending-message strip and the transcript row show the
  attachment the same way a fresh turn's message does (the
  attachment markers already exist).
- `decide::submit` stops refusing: no PasteInput-and-warn path for
  attachments. The Esc-discards-images behaviour stays.
- Every other steer producer keeps working: `serve`'s drive layer,
  `task_message` steering a child, queued-message replay after an
  aborted turn.

## Acceptance Criteria

- Attach an image mid-turn, send: the message is delivered as a
  steer with its image, the input clears, and the model's next step
  sees the picture. Same for a queued message once its turn ends.

## Milestone

13 — Guard rails
