# Compact while the cache is warm

## Summary

A session that idles past the provider's cache TTL turns every
possible next move into a cold re-read of the whole context —
continuing, compacting, even one small question all pay full price
on 700k tokens. But for a few minutes after the last request there
is a window where compaction is cheap: the prefix is still cached,
so the compaction request reads the giant context at cached rates
and leaves behind a small one. Fire it automatically in that
window and the inevitable expensive miss becomes a cheap one.

The strictly-better property: compaction is already non-destructive
(`Compaction { kept_from }` in the log, full history kept), so
"rewind to full context" is just reopening the window past the cut
— and the full-price turn that follows is exactly what doing
nothing would have cost. Worst case loses one summary's output
tokens; the common case saves an order of magnitude.

The unattended case is the strongest: a conductor that delegates
and waits forty minutes for children is *guaranteed* to go cold,
and it resumes automatically when results arrive — nobody is there
to choose. Today it silently pays the cold re-read every time.

## Requirements

- Config-gated, off by default: this fires unattended provider
  requests and reshapes context without a human present, so the
  user opts in — something like `[cache_compact]` with `enabled`,
  per-provider `ttl` (none of the providers contract their TTLs;
  OpenAI and GLM prefix caches live minutes, so default ~5m),
  a safety `margin` (default ~60s), and a `context_floor` (default
  ~150k tokens) below which a miss is pennies and summary fidelity
  is not worth spending.
- Trigger: `last_provider_request + ttl - margin`, only when the
  session is idle (a running turn refreshes its own cache), context
  is above the floor, and no delivery/steer/question is pending.
  Real work arriving first cancels the timer — the turn itself is
  the cache refresh.
- Both drivers: the TUI event loop (same pass-timer shape as the
  stall watchdog) and the serve engine, where the conductor case
  lives.
- Bounded and loud: one compaction per idle episode, no retry on
  failure (the window is gone anyway), a transcript line and a
  persistent notice: "compacted automatically to keep the cache
  warm — <key> rewinds to full context".
- The rewind affordance actually works: the banner's key reopens
  the pre-compaction window via the existing time-travel machinery,
  and the notice explains that continuing from there pays the full
  re-read the compaction avoided.

## Acceptance Criteria

- An idle session past the threshold compacts once, warm, and the
  next user turn runs on the compacted context at cached prices.
- A steer, delivery, or user turn arriving before the timer fires
  wins; no compaction happens.
- Rewind restores the full-context window and the session continues
  from it correctly.
- Disabled config → behavior identical to today, bit for bit.
- A conductor waiting on children compacts while waiting and its
  children's results steer into the compacted context.

## Notes

- Considered and rejected as the first move: cache keepalive pings.
  Each ping pays a cached read of the entire context, so an hour of
  absence costs more than one compaction and still holds the giant
  context. Only wins for short gaps where full fidelity is
  mandatory; could be a later config mode.
- TTL estimates are heuristic by nature — the feature must be
  harmless when the guess is wrong in either direction (fired late:
  it pays the cold read compaction would have paid anyway; fired
  early: it spent a cheap cached read sooner than needed).
- Watch interaction with [[the-root-turn-gets-a-watchdog]]: an
  auto-compaction is a legitimate silent phase; the watchdog's
  in-turn compaction exposure notes apply to this trigger too.
