# Shift-Enter inserts a newline

## Summary

The multiline prompt supports Ctrl-J for newlines, but Shift-Enter still submits.

## Requirements

- Insert a newline when Shift-Enter is reported by the terminal.
- Enable keyboard disambiguation on terminals that support it.
- Keep Ctrl-J as the portable newline fallback.
- Restore keyboard enhancement state during terminal cleanup.

## Acceptance Criteria

- Enter submits while Shift-Enter and Ctrl-J insert newlines.
- Unsupported terminals continue to start normally.
- Focused TUI tests and strict checks pass.
