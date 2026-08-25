# Nine pickers, one skeleton

## Summary

The picker pattern — query + `ListNav` + filtered view +
Esc/Enter/edit-key skeleton — is instantiated nine times in
modals.rs as separate structs (CommandPalette, SkillPicker,
SessionPicker, SessionSearch, LinkPicker, TurnPicker, ModelPicker,
VariantPicker, ThemePicker), roughly a third of the 3.6k-line file.
The copies have already diverged: Enter on an empty filtered list
dismisses four pickers and stays in three. The fuzzy-filter pipeline
is copy-pasted four times, the windowed row-render loop ~eight
times, the "filter " header three times.

## Requirements

- A shared `FilteredPicker<T>` (query + nav + filter fn) with hook
  points for per-picker bindings (arm-delete, fork, mode switch) and
  one windowed-row render helper.
- One decided, consistent Enter-on-empty behavior.
- The pending manager adopts `ListNav` instead of hand-rolled wrap
  (folded in with its scroll-window fix if that lands first).

## Acceptance Criteria

- All nine pickers behave as before (existing tests pass, updated
  where behavior was unified); the duplicated skeletons are gone.

## Milestone

12 — Health sweep

## Outcome

A private `Picker` trait plus shared free helpers (fuzzy/term
filter, row window, filter header, key skeleton, paste with
QUERY_CAP) — a trait rather than a struct so app.rs/main.rs field
access survives unchanged; per-picker chords stay ahead of the
skeleton. Enter on an empty filtered list unified to Stay across
all nine. Verified with a temporary golden differential harness:
pre-refactor renderers mounted from HEAD, cell-for-cell buffer
equality across sizes/queries/states, 30-key action scripts — only
divergence the deliberate Stay. (bb339e8)
