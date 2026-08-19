# Add theme picker

## Summary

Add several cohesive TUI color themes and a keyboard-driven menu that previews
the highlighted theme before confirmation.

## Requirements

- Provide a small curated set of readable themes that preserve existing layout
  and hierarchy.
- Expose theme selection through the command palette and a direct keyboard
  shortcut consistent with existing picker bindings.
- Preview the highlighted theme immediately while navigating.
- Confirming keeps the selected theme; cancelling restores the previously active
  theme.
- Persist the confirmed theme when the existing configuration architecture can
  support it without introducing a separate settings store.
- Keep every picker and transcript view bounded and usable on narrow terminals.

## Acceptance Criteria

- Theme menu lists all available themes and marks the active selection.
- Keyboard navigation visibly previews each highlighted theme.
- Escape restores the pre-menu theme and confirmation retains the previewed one.
- A confirmed theme survives restart when persistence is supported.
- Existing model, reasoning, command-palette, transcript, and status behavior is
  unchanged.
- Workspace tests, formatting, and clippy pass.

## Notes

- Preserve the current theme as one option and default.
- Color is presentation only; do not encode state solely through hue.
- Implemented five themes (`terminal`, `carbon`, `parchment`, `frost`, and
  `high-contrast`) as a final-buffer transformation over existing semantic ANSI
  colors, preserving transcript cache behavior and render call sites.
- Added `F3`, `Ctrl-X T`, and command-palette access. Navigation previews,
  Escape restores, and Enter persists the user-scoped preference atomically.
- Persistence uses syntax-aware TOML editing, preserves unrelated content and
  CRLF files, retries concurrent edits, and distinguishes uncertain durability.
- Centralized modal gating keeps notifications, paste, and mouse interaction
  behind every picker. Independent code and UX follow-up reviews found no
  remaining release blockers.
