# Skills inlined in the command palette

## Summary

Skills open via a submenu; inline rows make them first-class citizens
of Ctrl-P.

## Requirements

- Palette entries become dynamic (built at open time): built-in commands
  plus one row per skill under a "Skills" section, filterable together.
- Choosing a skill inserts `/name ` into the input (same as the picker).
- The `/` picker remains.

## Acceptance Criteria

- Tests: skills appear and filter; choosing inserts the prefix.
