# Hierarchical transcript activity

## Summary

The transcript currently renders tool and agent activity as a dense flat list.
Group related calls into expandable hierarchy while adding whitespace between
top-level events.

## Requirements

- Insert one blank row between top-level transcript items, but not between
  parent rows and their children.
- Group consecutive calls from the same provider step, while leaving a single
  call as a direct row.
- Show only active children beneath a collapsed running group and collapse a
  completed group to its call count.
- Expand groups and individual calls by clicking their header rows without
  breaking transcript text selection.
- Show bounded, sanitized arguments and results for expanded calls.
- Show active child thoughts and calls beneath agent rows.
- Expand completed agents to show their nested activity and final answer.
- Preserve grouping, details, and agent hierarchy when restoring a session.
- Keep reflow, scrolling, wrapping, caching, and narrow-terminal behavior
  stable.

## Acceptance Criteria

- Top-level rows have exactly one separator row and child rows remain compact.
- Active and completed call groups render the requested collapsed and expanded
  states.
- Tool details are bounded and cannot overflow the transcript viewport.
- Clicks toggle semantic transcript rows while drags continue selecting text.
- Agent activity is associated with the correct parent call, including parallel
  agents.
- Live and restored transcripts provide equivalent completed hierarchy.
- Existing transcript, session, event-channel, and subagent tests pass.
