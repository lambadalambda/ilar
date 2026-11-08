# Keep clipboard images with the submitted message

## Summary

Clipboard image processing runs on a worker. Enter can submit text before the worker completes, leaving the image for the next message.

## Requirements

- Do not send a message before its clipboard images finish processing.
- Keep image processing off the render task.
- Preserve the draft and report image-processing failures without silently sending text alone.
- Keep automatic continuation from consuming a draft while clipboard images are being prepared.

## Acceptance Criteria

- A controlled worker test proves that Enter cannot send ahead of image processing.
- Once processing succeeds, the submitted message has its images in paste order.
- Processing errors do not cause a text-only message or a lost draft.
- Relevant tests pass.

## Notes

- Enter during processing keeps the draft and shows a notice. The user presses Enter again after the attachment appears. It does not schedule a delayed send that could race with later edits.
