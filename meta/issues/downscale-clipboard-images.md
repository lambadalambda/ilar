# Clipboard images are downscaled before they enter the session

## Summary

A retina screenshot is 5120×2880 and megabytes of PNG, but providers
fit images into ~2048×2048 before tiling them anyway — everything
above that is upload bytes, session-file weight and cache-prefix bulk
for zero fidelity. The clipboard hands us decoded RGBA, so the fix is
arithmetic, not a native imaging library.

## Requirements

- Images whose longest edge exceeds 2048 px are downscaled to fit,
  aspect preserved, before PNG encoding — an area-average (box)
  filter over the RGBA buffer, no new dependency.
- Smaller images pass through untouched.
- The 10 MB cap stays as the backstop after downscaling.

## Acceptance Criteria

- Unit tests: dimensions map correctly (4000×2000 → 2048×1024), small
  images are untouched, averaging actually averages.
- A pasted oversized screenshot lands in the session at ≤2048 px.

## Milestone

11 — Beyond the terminal

## Outcome

`downscale_rgba` in the TUI: an area-average box filter over the
decoded RGBA the clipboard already provides, applied before PNG
encoding when the longest edge exceeds 2048 px. No new dependency —
vips was considered and rejected (native system library for what is
plain arithmetic on a buffer we already hold).

Verified live: a 5120×2880 clipboard paste landed in the session as a
2048×1152 PNG (19.5 KB against the 69 KB source), confirmed by
decoding the stored base64's IHDR. Unit tests pin the dimension
mapping, the pass-through for small images, and that averaging stays
within regions. The same experiment run answered the open caching
question: a follow-up turn over an image-bearing prefix read 64% of
its prompt from the provider cache.
