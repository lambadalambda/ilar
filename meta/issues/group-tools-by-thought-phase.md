# Group tools by thought phase

## Summary

Single-call provider steps still leave many standalone tool rows. Group ordinary
tools beneath the thought phase that initiated them and allow expanded calls to
reveal all captured detail after the compact preview.

## Requirements

- Render every run of ordinary tools as a group, including one-call runs.
- Treat a tools group immediately following a thought as that thought's compact
  child, without a blank separator row.
- Keep agent calls as separate top-level parents.
- Cycle tool disclosure through collapsed, bounded preview, and full captured
  detail states.
- Preserve viewport bounds, selection behavior, restoration, and cache
  invalidation.

## Acceptance Criteria

- Tool calls separated by thoughts no longer render as standalone tool rows.
- Thought-to-tools spacing is compact while separate top-level phases remain
  airy.
- A second expansion reveals content hidden by preview row limits.
- A third click collapses the call again.
- Existing transcript and workspace checks pass.
