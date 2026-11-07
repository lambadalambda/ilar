# Memory notes keep their evidence

## Summary

A persistent note records a claim but has no automatic reference to the experience that supports it. `Why` and `How to apply` explain a rule; they do not show what was checked.

## Requirements

- Define a small, backward-compatible representation for source references and verification state before changing the note format.
- Capture the source session and stable event reference or range for in-session writes, review plans and compaction flushes where available. A write event alone is not evidence for the claim it contains.
- Keep the original fact date distinct from edit and verification times. Creating, importing or editing a note must not mark it verified.
- Preserve source information when amending a note. Define when a changed claim invalidates its earlier verification.
- Treat old notes and Claude imports without transcript evidence as unverified with a known or unknown origin; do not invent sources.
- Expose enough source information through memory reads for a later validation pass. Source content remains data, not instructions.

## Acceptance Criteria

- Old note fixtures still load without migration or loss of their bodies.
- Round-trip tests cover source references and verification state.
- Tests distinguish creation, amendment and an actual evidence check, including a changed claim losing unsupported verification.
- Imported notes retain their origin without becoming verified, and repeated imports still respect forgotten notes.
- A reference resolves to the original evidence after compaction, or reports that the evidence is unavailable without silently substituting a summary.

## Notes

- Part of [the memory follow-ups](memory-follow-ups-from-the-component-review.md).
- Current note fields and amendment behavior: `crates/ilar/src/memory.rs:327-334,953-985`.
- Keep this a metadata and write-path change. Source search and automatic validation have separate issues.
