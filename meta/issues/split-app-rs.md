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

## Outcome

The render pass (`render`, `status_line`, `pending_strip_lines`,
activity/liveness helpers) moved to a new view.rs as `impl App`
blocks; the services panel (rows, exited disclosure, hover
underline) finished the sidebar.rs seam as pure functions with the
App-state decisions staying in `render`. app.rs 7452→~6300 lines.
Every moved function machine-diffed byte-identical against HEAD;
25 fields widened to `pub(crate)` were audited as all
view-referenced. `windowed_rate` went back to app.rs post-review
(stream accounting, its only caller). The image-pipeline part of
this issue had already landed via ilar::image (S1). The services
`safe_text` gap is recorded in sweep-cleanups.
