# Improve operational status

## Summary

Make ilar's status line communicate transient notices and context pressure with
the compact, aligned clarity of btop's meters.

## Requirements

- Render meaningful transient `App::status` notices instead of discarding them.
- Prioritize errors, pauses, cancellation, and shortcut prompts over secondary
  path and usage details.
- Add a compact context meter on sufficiently wide terminals while retaining the
  numeric percentage.
- Use discrete normal, warning, and critical states rather than decorative
  gradients.
- Keep narrow status lines width-bounded and useful.

## Acceptance Criteria

- Model failures, paused notifications, and shortcut prompts are visible.
- Wide status lines show a context meter and percentage.
- Narrow status lines retain activity or critical notice plus context percentage.
- Color is not the sole representation of context pressure.
- TUI and workspace checks pass.

## Notes

- Do not add historical sparklines until ilar stores meaningful per-step history.
- Prefer one primary animation locus in the status area; streaming redraw timing
  should remain unchanged.
