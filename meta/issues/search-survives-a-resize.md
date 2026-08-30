# Search survives a resize

## Summary

Search matches recompute only on `transcript_revision`
(view.rs:583-589); a terminal resize reflows the cache without
bumping it, so highlights tint the wrong rows and jumps land wrong
until the next transcript mutation.

## Fix

Key the recompute on (revision, width), or expose a cache
generation that resize bumps.

Size: S. Source: sweep 2026-08-29, rendering.

## Outcome

Matches are keyed on `(transcript_revision, width)`. Expand/collapse
already bumped the revision; a resize reflows the cache without
touching it, which is the hole this closes.
