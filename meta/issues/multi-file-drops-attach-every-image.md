# Multi-file drops attach every image

## Summary

Dropping several files at once pastes several paths in one go —
space-separated, each plain, quoted, or backslash-escaped, often
mixed. The single-path rule doesn't match, so the whole paste lands
in the input as text. A paste that tokenizes entirely into existing
image-file paths should attach them all.

## Requirements

- The paste is split shell-style (quotes and backslash escapes
  honored); it attaches only when *every* token is an image path and
  every path exists — one stray word and the paste stays text.
- Each file goes through the existing per-file pipeline (sniff,
  PNG downscale, gates).
- One summary notice for multi-file drops: all attached, or how many
  of how many; a total refusal keeps the per-image reason.

## Acceptance Criteria

- Unit tests: mixed-style multi-path split; a non-image token poisons
  the paste back to text; the attach loop lands all files in
  `pending_images`.
- Manually: dropping two images attaches two rows in the strip.

## Milestone

11 — Beyond the terminal

## Outcome

The single-path matcher became a small shell-style word splitter
(quotes and backslash escapes, `None` on newlines or dangling quotes)
feeding `dropped_image_paths`: every token must carry an image
extension and every path must exist, or the paste stays text. Each
file rides the existing pipeline; the attach fns now report success so
a multi-drop ends in one summary notice ("2 images attached", or
"attached N of M" on partial refusal, with a total refusal keeping the
per-image reason).

Pinned by tests (mixed-style splitting, poison tokens, a real
two-file attach through the paste intent, missing-file fallback to
text) and verified live: a bracketed paste of `/tmp/one.png
/tmp/two\ shots.jpg` attached both, two rows in the strip.
