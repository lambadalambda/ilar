# Rate limits are waited out

## Summary

A 429 is classified retryable, but a turn gets the same budget for it
as for a flaky 502: three attempts at 0.5 s doubling — about 3.5 s of
patience — and the server's `Retry-After` is never read. On a
subscription with usage caps (OpenCode Go: "Rate limit exceeded.
Please retry after a brief wait.") that surfaces as a failed turn the
user has to resume by hand, seen 2026-09-03 on muse-spark-1.3.

## Requirements

- The transport reports a rate limit as its own event, carrying
  `Retry-After` (seconds) when the response names one.
- The loop gives rate limits (429, and 529 overloaded) a separate,
  longer budget: more attempts, longer doubling delays with a higher
  cap, and the server's hint when given, clamped. Ordinary transient
  errors keep today's budget; the two counters are independent.
- `ProviderRetry` reports the budget that applies, so the TUI's
  "attempt a/b, retrying in Ns" stays truthful.
- Still only before any response content arrives — mid-stream recovery
  is [A turn continues after a hiccup](a-turn-continues-after-a-hiccup.md).

## Acceptance Criteria

- Transport: a 429 with `Retry-After: 7` yields a rate-limit event
  carrying 7 s; without the header, none; a 503 stays a plain
  retryable error.
- Loop: five consecutive rate limits followed by success complete the
  turn (today the fourth fails it); the delay follows `Retry-After`
  when present; the generic budget is untouched by rate-limit retries.

## Outcome (2026-09-03)

The transport now reports 429 and 529 as `ProviderEvent::RateLimited`,
carrying `Retry-After` in its delay-seconds form (capped at five
minutes; the date form reads as absent). The loop keeps two counters:
transient errors get the old three at 0.5 s doubling, rate limits get
six at 2 s doubling capped at 60 s — 2, 4, 8, 16, 32, 60, about two
minutes — with the server's hint as a floor when it names one.
`ProviderRetry` carries whichever budget applies. Still only before
response content arrives. Covered by transport tests (hinted, bare,
dated, oversized, and a 503 staying ordinary) and a loop test in which
five 429s then a success complete a turn that one transient retry
would have failed.
