# One delivery engine

## Summary

serve's Consumer re-implements follow-up-vs-route, backoff, hops
and delivered-checks beside the TUI's path — and has already
diverged (no retire, no salvage, adoption-once: three filed bugs).
The delivered-predicate exists in three copies. Extract the
delivery loop into core so both drivers fold the same rules, and
give serve the watchdog it currently lacks entirely (a wedged
provider stream holds a serve slot until a human notices).

Size: L. Source: sweep 2026-08-29, serve + subagent (found from
both sides independently).

## What landed (2026-08-30)

The engine exists: `ilar::delivery`, holding the two things every
driver has to agree about.

- **One delivered predicate.** `is_delivered` / `delivered_in`, with
  the substring rule and its byte-identical-duplicates limitation
  written down once. The three hand-written copies — `route_notification`,
  `outbox::pending`, serve's `already_delivered` — all call it now.
- **One reading of an ending.** `Disposition::{Delivered, Propagate,
  Exhausted, Hold, Salvage}` and `disposition()`. A driver matches it
  exhaustively, so forgetting to salvage or to retire is a compile
  error rather than a completion quietly re-announced and re-failed at
  every start.
- **One climb budget.** `Parcel` carries the hops; `PROPAGATION_HOPS`
  is shared. This closed a real asymmetry: serve bounded the climb, the
  TUI did not, so a parent chain that loops (the case `outbox`'s
  ancestry cap already refuses to walk) would have propagated forever
  in the terminal. A spent budget is `Exhausted`, which the TUI
  salvages and retires rather than dropping — serve's old behaviour was
  a silent drop with a log line.

  Two details worth writing down, both found in review. serve's budget
  gained a hop: its old test was `hops <= 1` *after* routing, which
  spent one climb without counting it, so eight hops bought seven.
  `climbing` refuses only at zero, which is what the constant's name
  says. And `Exhausted` must carry the notification the *last* attempt
  produced, not the one the parcel arrived with — the arriving one was
  already appended to the log of the session that hop reached, while
  the new one is the entry now stranded in the outbox. Getting that
  backwards salvaged the wrong text and tombstoned a file that no
  longer existed, leaving the real entry to be re-adopted with a fresh
  budget at every start: the exact forever-loop the budget exists to
  stop.

## What remains — parked with serve

serve's `Consumer` still owns its own follow-up-vs-route decision, its
own retry limits and backoff, adoption-once-per-process, and no
watchdog at all. Those are the shape of the driver, not the rules it
folds, and rewriting a dormant driver is the tax
[[serve-steps-out-of-the-default-build]] was meant to stop paying. When
the feature wakes, its loop folds through `Disposition` — the enum is
already there, and the compiler will name every ending it forgot.
Related: [[serve-retires-what-it-cannot-route]],
[[serve-joins-the-turns-it-started]].
