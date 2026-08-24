# Finish modal scaffold adoption

## Summary

Two stragglers from the picker-core extraction: `render_pending_manager`
and `render_help` still hand-roll the frame scaffold that `modal_frame`
now owns, and the pending manager builds its click map separately from
its lines instead of through `ModalRows`. Also fold in the one
remaining paste-path nit: `CommandPalette::insert_query` resets the
selection even when the pasted text was entirely control characters.

## Requirements

- Both renders go through `modal_frame`; the pending manager builds
  rows through `ModalRows`. Rendered output is unchanged (existing
  render tests must pass as-is).
- `insert_query` resets the selection only when it actually changed the
  query, matching the keyboard path's behaviour after the extraction.
- No other behaviour changes.

## Acceptance Criteria

- No hand-rolled `Block::default().borders(...)` scaffolds remain in
  modals.rs outside `modal_frame`.
- A test pins the all-control-characters paste as a full no-op.
- The full suite passes.

## Outcome

Done as specced; the paste no-op is pinned by a test, and the two
renders now share the scaffold, leaving `modal_frame` as the single
frame construction in the file.

## Milestone

9 — Time travel follow-ups
