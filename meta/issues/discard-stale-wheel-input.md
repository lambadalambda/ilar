# Discard stale wheel input

## Summary

Rapid mouse-wheel input can remain queued and continue scrolling long after the physical wheel stops, making the transcript appear glued to the top or bottom while the backlog drains.

## Requirements

- Do not replay stale mouse-wheel events over subsequent render cycles.
- Coalesce or drain immediately available wheel input so the viewport reflects the latest physical gesture promptly.
- Preserve wheel direction changes, keyboard scrolling, selection behavior, and terminal responsiveness.

## Acceptance Criteria

- A burst of queued wheel events is handled as one current input batch rather than delayed scrolling over time.
- Opposing wheel directions in the same batch produce the corresponding net movement without overshooting.
- A zero-net wheel batch leaves the viewport unchanged but still cancels transcript selection.
- Wheel draining is bounded so rendering, streaming, and notification work cannot starve.
- The first non-wheel event remains ordered ahead of any later queued terminal input.
- Existing scrolling, rendering, and input tests continue to pass.
