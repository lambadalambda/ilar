# Allow image attachments while a turn runs

## Summary

The TUI refuses new image attachments while a turn runs, although steering and queued messages already carry images.

## Requirements

- Accept clipboard images and dropped image files during a running turn on vision-capable models.
- Keep the existing model and image-size checks.
- Keep images with their submitted message through steering, queueing and queue delivery.
- Do not let automatic continuation take images from an unsubmitted draft.

## Acceptance Criteria

- A regression test attaches an image while busy and sends it through the queue after the running turn completes.
- A mid-turn attachment can also reach the steer channel, including its queue fallback.
- An unsubmitted image stays pending when completion could start a goal round or send an earlier queued message.
- Relevant TUI tests pass.
