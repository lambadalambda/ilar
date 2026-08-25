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
- Substantial: touches both providers' wire formats, the store,
  and the TUI. Needs its own design pass before implementation.

## Milestone

12 — Health sweep
