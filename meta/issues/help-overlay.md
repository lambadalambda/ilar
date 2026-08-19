# Help overlay with keybindings

## Summary

Keybindings (Ctrl-X leader, F2/F3, palette, folding clicks, scroll keys)
are discoverable only via a one-line banner. There is no help screen.

## Requirements

- F1 (and `?` when the input is empty) opens a modal overlay listing all
  keybindings grouped by area (input, transcript, pickers, session).
- The overlay is generated from a single static table that the key
  dispatcher also references where practical, so help cannot silently
  drift from behavior.
- Esc or any listed toggle key closes it; theme-aware styling.
- Add a palette entry "Help".

## Acceptance Criteria

- Every binding handled in the key dispatcher appears in the overlay
  (spot-checked by a test over the static table).
- Overlay renders within the smallest supported terminal size without
  panicking.

## Notes

- Plain scrollable list is fine; no search needed.
