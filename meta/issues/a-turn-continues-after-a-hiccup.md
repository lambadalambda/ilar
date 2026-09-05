# A turn continues after a hiccup

## Summary

Any transient failure after the first delta fails the whole turn
(retry correctly gates on received_response), and recovery is a
manual Resume — five minutes of idle-timeout dead air, then an
error. Everything for safe automatic continuation exists: the
partial step persists with synthetic tool results, the error is
classified retryable, resume_turn continues from the accumulated
transcript. This is caller policy, not machinery: bounded
auto-resume (once or twice, marked in the transcript) for
retryable mid-stream failures.

Size: M. Source: sweep 2026-08-29, core loop.

## Outcome (2026-09-05)

In the loop, not the caller, so exec and serve get it too: a
retryable failure (transport, 5xx, rate limit) after content has
streamed commits the partial step the way the failure path already
did — the streamed blocks, a `step interrupted: … — continuing`
diagnostic, an error result per announced tool call — publishes
`LoopEvent::StepInterrupted { attempt, max_resumes, error }`, waits a
beat (the server's own wait for a rate limit), and lets the step loop
issue the next request from the transcript. `max_mid_stream_resumes`
is 2 per turn; past it, and for a permanent error, the turn fails
exactly as before with Ctrl-R to resume. The TUI shows the seam as a
transcript line and a transient warning; exec prints and emits it.
Tests: a hiccup after the first delta completes the turn in two
requests with the second continuing from the committed text; the
budget holds and a permanent error never continues. Also fixed on
the way: the retry notice was no longer cleared by incoming data
after its wording changed.
