# Enter submits a fully typed slash command

## Summary

With the completion popup open, Enter always *completes* the selected
candidate. Typing `/sessions` in full and pressing Enter appends a
trailing space instead of submitting; every argument-less command
costs a second Enter.

## Requirements

- When the input already equals the selected candidate exactly
  (`/name`, no arguments), Enter submits through the normal prompt
  path. Tab keeps completing.
- A partial name (`/ses`) keeps today's behavior: Enter completes.

## Acceptance Criteria

- A test pins both behaviors.
- The full suite passes.

## Milestone

11 — Beyond the terminal
