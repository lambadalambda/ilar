# Images to and from the phone

## Summary

Attach photos to a prompt from the phone, and show the images of the
transcript on the phone (today: "not sent to the phone").

## Requirements

- Upload: the phone scales a photo down and sends it with the prompt;
  the wire's 256 KiB command limit grows to fit.
- Display: the phone asks for an image by its descriptor and the
  session sends the bytes.

## Acceptance Criteria

- A photo taken on the phone reaches the model in the next turn, and
  the transcript on the phone shows it.

## Status

Built 2026-11-27: `FromDevice::Prompt.images` (checked by the terminal's
own `image_refusal`; a start stages them in `staged_turn_images`, not in
the draft), `FromDevice::Image` / `ToDevice::Image` answered from the
feed's log, the wire's device limit at 16 MiB. The phone's composer has
a photo button (shrunk to 1600 px JPEG). Checked on the iOS simulator
against a live TUI: a photo went to Kimi K3.1 Pro, which described it
correctly, and the phone showed it under the message. A photo sent to
a model without vision is refused with a notice.
