# Polish transcript hierarchy

## Summary

Improve scanning of tool and agent activity with restrained btop-style alignment
and tree structure without reducing code or transcript width.

## Requirements

- Add subtle tree rails or branch markers to nested tool and agent rows when
  width permits.
- Keep tool names in primary text and apply semantic color to state indicators.
- Align wide tool rows into stable type, name, state, and summary regions.
- Preserve the existing compact flowing representation on narrow terminals.
- Keep transcript text selection, click targets, disclosure, and restoration
  behavior unchanged.

## Acceptance Criteria

- Parent/child relationships are easier to follow at a glance.
- Running, waiting, successful, and failed calls remain textually distinguishable.
- Hierarchy chrome never causes rows to exceed the viewport.
- Existing transcript cache and interaction tests pass.

## Notes

- Do not add a border around every transcript item.
- Do not use row inversion for live activity because inversion already indicates
  picker or transcript selection.
- Avoid decorative animation in rails, borders, or elapsed-time fields.
