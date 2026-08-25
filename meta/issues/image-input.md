# Image input: paste a screenshot, the model sees it

## Summary

No layer of ilar can carry an image: `ContentBlock` has no image
variant, providers never emit image parts, the catalog does not know
which models have vision, and the TUI has no way to take an image in.
On a vision model (all cataloged OpenAI models, GLM's V-series) a
screenshot is often the fastest way to explain a problem.

## Requirements

- `ContentBlock::Image { media_type, data }` (base64 inline in the
  session JSONL, bounded) and an `images` field on `UserMessage`,
  serde-defaulted so old sessions read unchanged.
- The model catalog knows vision: every cataloged OpenAI model
  supports image input; on z.ai only the V-series does.
- OpenAI Responses wiring: user text and images travel as parts of one
  message item (`input_text` + `input_image` data URL); text-only
  messages keep their exact current wire shape (cache prefixes must
  not move).
- z.ai wiring degrades gracefully: an image block becomes a text
  placeholder naming what was omitted. Proper GLM-V image parts are a
  follow-up.
- TUI: Ctrl-V with an image on the clipboard attaches it to the next
  message (PNG-encoded); attachments are listed above the input until
  sent; the user transcript row shows an image marker, also after
  restore.
- The vision flag gates the door: attaching on a non-vision model is
  refused with a notice naming the model, and submit re-checks in case
  the model changed after attach.
- Attachments ride a fresh turn only: submitting with attachments
  while a turn runs is refused with a notice (steer/queue carry text
  only).

## Acceptance Criteria

- Unit tests: vision flags per provider; transcript builds image
  blocks from a user event; OpenAI wire shape for text+image and
  unchanged shape for text-only; zai placeholder; attach/submit gating.
- Manually: paste a screenshot on a vision model, ask about it, get an
  answer that proves the model saw it.

## Notes

- Out of scope for this slice: the `read` tool returning images, GLM-V
  proper image parts, `ilar exec --image`, terminal image rendering.

## Milestone

11 — Beyond the terminal

## Outcome

Shipped across five commits: `supports_vision` on the catalog (all
OpenAI, z.ai V-series only); `ContentBlock::Image` +
`UserMessage.images` (serde-defaulted both ways — old logs read, new
image-free logs stay byte-identical); `run_turn` threads images into
the appended event; OpenAI sends `input_text` + `input_image` parts in
one message item with the text-only wire shape pinned unchanged by
test; zai degrades to a named `[image omitted]` text gap; the TUI
attaches via Ctrl-V (PNG-encoded through arboard + png), gates on
busy/vision/10 MB with naming notices, lists attachments in the
pending strip, and marks the transcript row.

Verified end to end: a generated red/blue "ILAR 42" test image pasted
from the real macOS clipboard into a live gpt-5.6-sol session came
back as "Red, blue; 'ILAR 42'" — and the marker survives
`--continue`. Unit tests cover every gate and wire shape.
