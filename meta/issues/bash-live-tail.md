# Live bash output tail

## Summary

Running bash rows show only elapsed time; long builds are opaque until
completion.

## Requirements

- Tools can report bounded output tails through ToolContext; bash
  reports the last lines of combined output periodically.
- The loop forwards tails losslessly-coalesced (latest wins) like input
  progress; the TUI shows the tail in expanded running tool rows.
- Bounded (a few hundred chars); no behavior change for other tools.

## Acceptance Criteria

- Executor/loop test that a long-running bash surfaces a tail before
  completion; TUI render test for the tail line.
