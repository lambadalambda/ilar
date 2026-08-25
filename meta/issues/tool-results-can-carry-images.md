# Tool results can carry images

## Summary

Vision subagents cannot actually see: task prompts are text-only,
read returns bytes (soon a description), and webfetch rejects
file:// URLs — so a GLM-4.5V child correctly reported it had no way
to view a PNG it was asked about (terranigma session). The image
plumbing exists end-to-end for *user* messages; tool results are
text-only on the wire.

## Requirements

- `ToolResult` can carry image content; the wire builders emit it
  (Anthropic: image source blocks in tool_result; OpenAI: an
  image part alongside the result text) for vision models, and a
  named `[image omitted]` gap otherwise.
- The read tool, pointed at an image file in a vision-model
  session, returns the image (downscaled per the existing
  `MAX_IMAGE_DIM` pipeline) with a one-line text description.
- Session log, compaction, recall, and the TUI transcript handle
  image-bearing tool results (size markers like user images).

## Notes

- This is the enabler for "spawn a vision subagent to look at this
  screenshot" — the workflow the terranigma session wanted.

## Design (verified 2026-08-25, live probes on all three routes)

Wire shapes — all three providers carry tool-result images natively,
no synthetic-user-message fallback anywhere:

- OpenAI Responses (verified on the ChatGPT/Codex backend; public
  endpoint assumed identical): `function_call_output.output` becomes
  an array `[{"type":"input_text",...},{"type":"input_image",
  "image_url":"data:..."}]`. Text first.
- zai OpenAI flavor: `role:"tool"` content array
  `[{"type":"text",...},{"type":"image_url","image_url":{"url":
  "data:..."}}]`. Text first.
- (The zai Anthropic flavor and its image-first ordering asymmetry
  are moot: the flavor was removed wholesale in be56531. Two wire
  shapes remain.)

Rules: text-only results keep today's plain-string form on every arm
(no cached prefix moves, old sessions byte-identical); non-vision
models get the existing `[image omitted]` gap, gated per request so
mid-session model switches degrade gracefully; images never upscale
and never shrink below a sane floor (a 4x4 probe image was invisible
to the model, 64x64 was read perfectly).

Session shape: `ToolResult` (event + content block) gains
serde-defaulted `images: Vec<ImageContent>` like `UserMessage`;
`transcript_of` forwards them; compaction counts base64 length (the
highest-value line — screenshots must trigger compaction on
schedule); recall keeps skipping image payloads; `settled()` and
replay/fork/rewind unaffected. `ToolOutput::with_images` enforces a
5 MiB decoded per-result cap, dropping over-cap images with a note.

Slices (each independently committable):
- S1 `image.rs` in core: move downscale/encode/sniff pipeline out of
  ilar-tui app.rs; `png` dependency moves to core.
- S2 session shape (model/event/store/compaction + mechanical
  construction sites).
- S3 providers (needs S2): the three wire shapes + ordering tests +
  plain-string regression + named-gap test.
- S4 tools (needs S2): `ToolOutput::with_images` + cap;
  `ToolContext::vision` set in run_turn from `supports_vision`.
- S5 read (needs S1+S4): image file + vision session → description
  ("the image itself follows") + downscaled image; every failure
  degrades to today's description.
- S6 TUI+docs (needs S2+S4): one `image::markers` helper feeding the
  live ToolFinished path, session_view replay, and
  user_text_with_images; docs/interface.md + agents-and-skills.md.

Open: public-Responses parity unprobed (assumed); repeated reads of
the same image are not deduplicated (defer until it bites).

## Milestone

12 — Health sweep
