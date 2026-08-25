# Split app.rs

## Summary

app.rs is 7.5k lines. ~1,300 of them are pure view code inside the
state struct's file (`render`, `status_line`, `pending_strip_lines`,
`activity_line`/`stream_liveness`), and a ~260-line self-contained
image pipeline (`dropped_image_paths`, `split_shell_words`,
`image_media_type`, `downscale_rgba`, `encode_png`, …) has zero
coupling to App beyond three notice-emitting methods. The sidebar
seam is half-done: agents/todos rows live in sidebar.rs, but the
services panel is built inline in `render`. Also: the doc comment
for `close_running_tools` is attached to `MAX_IMAGE_DIM` (a botched
code move — rustdoc for an image constant describes abort
semantics).

## Requirements

- Extract the render pass to a `view.rs` (or equivalent), finish the
  sidebar extraction for the services panel.
- Extract the image pipeline to an `images.rs` module.
- Reattach the stray doc comment.

## Acceptance Criteria

- No behavior change (existing tests pass); app.rs shrinks by
  roughly the extracted line count.

## Milestone

12 — Health sweep
