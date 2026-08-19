# Unify the TUI modal layer

## Summary

Six modals (`CommandPalette`, `ModelPicker`, `VariantPicker`,
`ThemePicker`, `SessionPicker`, `SkillPicker`) plus help, the pending
manager, and search are each an independent `Option<T>` field on `App`,
each with its own bespoke `*Action` enum and its own dispatch arm in
`run_app`. Five of the six `handle_key` implementations have
copy-pasted `Esc`/`Enter`/`Up`/`Down`/`Ctrl-P`/`Ctrl-N` arms over a
`selected` index and a `move_selection` helper. The dispatch chain they
drive is roughly 220 lines of near-duplicate matching.

Three concrete defects fall out of that shape:

**`has_modal()` is a hand-maintained 8-way OR** (`main.rs:2681`) that has
already drifted — it omits `search_active`. So while the search bar is
open: paste lands in the message input instead of the query, the wheel
scrolls the transcript, and a background notification can start a turn
underneath the user. `run_app` compensates with a hand-written
`&& !app.search_active` in three places and misses it in the
notification gate at `main.rs:7817`.

**Modals are keyboard-only.** `Event::Mouse(mouse) if !app.has_modal()`
(`main.rs:8532`) drops every mouse event while a modal is open. The model
picker lists 45 entries and can be neither scrolled nor clicked, in an
app where the transcript is otherwise click-to-expand and drag-to-select.

**Two hand-maintained precedence orders.** Render precedence
(`main.rs:4386`) is model → variant → theme → session → help → pending →
skill → palette; key-dispatch precedence (`main.rs:7916`) is pending →
help → theme → skill → session → model → variant → search → palette.
They are near-opposite. Nothing currently opens two modals at once (the
palette clears itself before activating a command), so today the app
renders and types into the same modal by luck rather than construction.

## Requirements

- A single `Modal` enum owns the active overlay, replacing the parallel
  `Option<T>` fields and `search_active`. One value, one precedence.
- Render and key dispatch derive from that one value, so the two orders
  cannot diverge.
- Shared list-navigation behaviour (selection index, wrap, Home/End,
  Ctrl-P/Ctrl-N) lives in one place; per-modal code keeps only what is
  genuinely specific (session delete/fork, theme live preview, variant
  no-op-on-unchanged).
- Mouse events route to the active modal: wheel scrolls its list, click
  selects, click outside dismisses.
- Search participates in modal gating like every other overlay, which
  removes the three ad-hoc `!app.search_active` checks.

## Acceptance Criteria

- Test: with search open, a paste event reaches the search query and not
  the message input.
- Test: with search open, the notification gate does not start a turn.
- Test: wheel and click events change the selection in an open picker.
- Exhaustive `match` on `Modal` in both render and dispatch, so adding a
  modal without wiring both is a compile error.
- Existing picker behaviour tests still pass unchanged.

## Notes

- This is a refactor with three small behaviour fixes riding along; keep
  it separate from the input-hazard fixes so each commit stays reviewable.
- Precedence should be decided explicitly and written down once — the
  current de-facto order is an accident, not a design.

## Milestone

6 — Hardening
