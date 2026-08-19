# Readline-style input editing

## Summary

The prompt input supports only character-wise motion and deletion. Standard
readline chords are absent.

## Requirements

- Ctrl-A / Ctrl-E: start / end of line.
- Ctrl-K: kill to end of line; Ctrl-U: kill to start of line.
- Ctrl-W: delete previous word; Alt-B / Alt-F: word-wise motion.
- Word boundaries: whitespace-delimited with punctuation treated as its own
  class (match readline's default closely enough to feel familiar).
- Multiline-aware: line-scoped chords operate on the current visual line.
- Do not break existing bindings (Ctrl-U currently half-page-scrolls when
  the input is empty; input-editing takes precedence only when the input
  is non-empty — document the rule).

## Acceptance Criteria

- Unit tests for each chord incl. unicode (multi-byte, combining) safety.
- Existing scroll bindings still work with an empty input.

## Notes

- Kill-ring/yank is out of scope; plain deletion is fine.
