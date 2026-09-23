# The title is the topic

## Summary

The TUI sets the terminal title to `ilar — <topic>`. The prefix costs
the width of a tab label, which is where the topic has to fit, and
says nothing the window does not already show. The user asked for it
gone.

## Requirements

- With a topic, the title is the topic alone.
- Without one, it stays `ilar`, so the terminal does not keep a stale
  title from before.

## Acceptance Criteria

- The title test pins both cases.

## Notes

- Source: user request, 2026-09-23. Size: XS.
