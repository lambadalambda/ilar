# A tool call has an argument cap

## Summary

The runaway was a `message` call whose arguments streamed forever.
No tool call's arguments are legitimately a megabyte; the loop
should cut the response there and report it, whatever the output
cap allows.

## Requirements

- While a tool call's arguments stream, more than 1 MiB ends the
  response with an error naming the tool and the size.

## Acceptance Criteria

- Test: a scripted provider that streams arguments past the cap
  produces a failed turn with that message, and nothing runs.
