# Server-side compaction for OpenAI

## Summary

Codex compacts OpenAI sessions on the server: a `/responses` request
with a `compaction_trigger` item returns an opaque `compaction` item
with `encrypted_content`, which replaces assistant, reasoning and tool
items in the history (`core/src/compact_remote_v2*.rs`). The model
keeps its own latent state across the cut. ilar has only the local
text summary, and in session aed7fe15 the summaries carried damaged
text forward through 19 compactions.

## Requirements

- Investigate the wire (ChatGPT backend and API key), what Zen/Go
  accept, and how the item lives in ilar's log next to the summary.

## Acceptance Criteria

- A design note in this file before any code.
