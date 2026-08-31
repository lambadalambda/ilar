# Images are bounded before decoding

## Summary

Image ingestion applies its limits too late. A dropped file is read and base64-encoded before the TUI's 10 MiB cap, and a small compressed PNG can declare dimensions that make `downscaled_png` allocate a very large decoded buffer before downscaling. The read tool's compressed-size guard does not bound decoded dimensions either.

## Requirements

- Define explicit compressed-byte, pixel-count, and decoded-byte limits for image ingestion.
- Bound dropped-file bytes before reading or encoding the whole file.
- Reject file images whose dimensions or decoded byte count exceed the limit before allocating the frame buffer.
- Apply the same decoded-image policy to read-tool attachments and check clipboard dimensions immediately after the clipboard library hands over its already-decoded buffer.
- Fail with a useful tool result or notice rather than panicking or exhausting memory.

## Acceptance Criteria

- Tests cover an oversized dropped file and a small PNG with pathological declared dimensions.
- No file-based image path allocates the full payload or decoded frame before the applicable bound is checked.
- The unavoidable allocation performed inside clipboard acquisition is documented; ilar makes no additional unbounded allocation from it.
- Normal supported images and ordinary downscaling continue to work.

## Notes

- Source: `crates/ilar-tui/src/app.rs:1826-1877`, `crates/ilar/src/image.rs:70-107`, `crates/ilar/src/tools/read.rs:198-234`.
- Found by the current codebase review.
