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
