# The TUI remembers on its own

## Summary

Once the store lives in the core, a TUI session has the memory tools,
but nothing tells the model when to use them. The gateway has an
after-turn reviewer for that; the TUI has a present user and a
different pace, and a reviewer call after every turn is the wrong
tool. Claude Code's answer is a standing instruction: the model
writes memory itself, during the turn, when it learns something
worth keeping. The TUI takes the same approach.

## Requirements

- A prompt section, injected once at session open beside the core
  block, tells the model what memory is for and when to write it:
  preferences, corrections, decisions and conventions; not what the
  repo, the session log or a search already records. It is present
  whether or not the directory has memory yet, so an empty store
  gets written.
- The `memory` tool's description and the prompt section both say
  how to write a note's summary: the one line that answers "what
  question does this file answer", carrying the tokens a future
  prompt would contain (ticket ids, hostnames, error strings, file
  names). The index matches on those tokens.
- The same summary guidance goes into the gateway's after-turn
  review prompt, so notes written there are found the same way.
- The core block's "frozen for this session" note stays; a write
  during the session changes the next session's prompt, not this
  one's.
- The off switch, `[general] memory = false`, landed with the store
  (it removes the tools and the core block); it removes the prompt
  section too.

## Acceptance Criteria

- A prompt test shows the section present in a fresh directory with
  no memory, and the core block absent until something is written.
- A wire test shows a `memory` write mid-session leaving the request
  prefix byte-identical for the rest of that session.
- The `memory` tool description names the summary rule; the gateway
  reviewer's prompt carries the same sentence.
- docs cover the prompt section and the off switch.

## Notes

The section is the terminal session's (`RuntimeOptions.memory_prompt`);
the gateway leaves it off, since its review after a turn writes for
it, and the review's prompt carries the summary rule instead.

Sub-issue 2 of 3, after [the-memory-store-moves-into-the-core].
Whether the TUI ever gets a reviewer of its own is a separate
question; nothing here rules it out.

Size: S. Source: comparison with Claude Code auto-memory, 2026-09-18.
