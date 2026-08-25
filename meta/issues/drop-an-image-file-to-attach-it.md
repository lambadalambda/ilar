# Drop an image file to attach it (png, jpeg, webp, gif)

## Summary

Terminals turn a drag-and-drop into a pasted file path, which today
just lands in the input as text. When that pasted text is the path of
an image file, the natural reading is "attach this". Ctrl-V only
covers clipboard bitmaps (always PNG-encoded from RGBA); dropped files
bring their own formats, and every provider we speak accepts png,
jpeg, webp and gif — so original bytes can pass through untouched.

## Requirements

- A paste that is exactly one existing image-file path (plain, quoted,
  or backslash-escaped, as the common terminals emit) attaches the
  file instead of inserting the path; any other paste behaves as
  before.
- The media type comes from the bytes (magic numbers), not the file
  extension; unrecognized bytes fall back to inserting the path.
- JPEG/WebP/GIF pass through as-is. PNGs larger than 2048 px on the
  longest edge are decoded, downscaled like clipboard images, and
  re-encoded; PNGs the decoder cannot normalize pass through.
- The same gates as Ctrl-V apply: busy, vision, 10 MB.

## Acceptance Criteria

- Unit tests: path detection for the three quoting styles and the
  non-path negatives; magic-number sniffing; oversized-PNG downscale;
  jpeg pass-through.
- Manually: dropping a PNG onto the terminal attaches it and the model
  answers about its content.

## Milestone

11 — Beyond the terminal

## Outcome

`dropped_image_path` recognizes the three quoting styles terminals
emit (plain, quoted, backslash-escaped) and nothing else — prose,
multiline pastes and non-image extensions still land in the input;
only a path that also *exists* attaches. Bytes are sniffed by magic
numbers; jpeg/webp/gif pass through byte-identically with their own
media type, and PNGs above 2048 px decode (normalized to 8-bit
gray/rgb/rgba), downscale through the same filter as clipboard
images, and re-encode — undecodable PNGs pass through. The Ctrl-V
gates (busy, vision, 10 MB) apply unchanged.

Verified live: a bracketed-paste of `/tmp/my\ drop.jpg` (exactly what
a terminal drop emits) attached as `jpeg · 5.3 KiB`, the session
stored genuine JPEG bytes (`ffd8ff`), and gpt-5.6-sol answered
"Orange background with black text reading 'JPG 5'".
