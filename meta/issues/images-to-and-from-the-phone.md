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
