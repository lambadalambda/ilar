# Extract the picker core

## Summary

Eight picker modals hand-roll the same core: a `{items, query,
selected}` struct, clamp-on-select / wrap-on-move navigation, query
editing that resets the selection, the five-line scroll-window
expression (seven identical copies), and the render scaffold (centered
rect, clear, double border, title, footer, zero-size bail, `ModalHit`
rows built three incompatible ways). The first unification pass
(`unify-tui-modals.md`) fixed the dispatch layer and deliberately left
this; two more pickers have joined since.

## Requirements

- **Pin before touching**: render and key tests for the four unguarded
  pickers (`LinkPicker`, `SkillPicker`, `TurnPicker`, `SessionPicker`)
  land first and must pass unchanged before and after the extraction —
  including a click-row test for the session picker, whose rows vector
  is built post-hoc from `lines.len()` and is the most fragile spot.
- A small embedded selection core (composition, not a trait): clamp on
  `select`, `rem_euclid` wrap on `move_selection` (the wheel relies on
  wrapping), reset hooks staying in the wrapping picker
  (`pending_delete`, `armed`, `error`).
- One `list_window(selected, len, rows)` helper replacing the seven
  copies of the scroll-window math.
- One query-editing helper. Two deliberate, called-out behaviour
  upgrades while unifying: Backspace becomes grapheme-aware everywhere
  (today session/turn/link pop bytes), and the control-character guard
  applies everywhere (today `ModelPicker` lacks it). Both get tests.
- One render scaffold for the frame (clear + border + title + footer +
  bail) and one row-collector that builds lines and `ModalHit` rows
  together, eliminating the three divergent constructions and the
  session picker's latent click off-by-one.
- Bespoke behaviour is preserved exactly: the palette's substring-AND
  filter, `ThemePicker`'s cached matches / whole-list fallback /
  re-anchoring / live preview, `VariantPicker`'s synthetic row and
  clamp-to-len, `ModelPicker`'s PageUp/PageDown and no-op-Enter rules,
  stable-sort recency preservation on empty queries, and every
  existing render test's exact output.

## Acceptance Criteria

- The full suite passes with no changes to existing tests other than
  those the two called-out upgrades require.
- Every picker's `select`/`move_selection`/query-edit path goes
  through the shared core; the seven window-math copies are gone.
- `grep -c "rem_euclid" modals.rs` is 1.

## Notes

- Explicitly out of scope, per the first issue's reasoned declines:
  collapsing the `*Action` enums, moving state into the `Modal` enum,
  click-outside-to-dismiss, and unifying the non-uniform Dismiss
  cleanup (the `VariantPicker`/`ModelPicker` status drift is flagged,
  not fixed).
- The `{text:<width$}` char-count-vs-display-width padding quirk is
  also out of scope: cosmetic, clipped by ratatui, and touching it
  perturbs the exact-text render tests.

## Milestone

9 — Time travel follow-ups
