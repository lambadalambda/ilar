# Fetch less, guess less

## Summary

Store-wide scan (2026-08-26, 1,612 webfetch calls): 32% error rate,
60% on first use — 321 are 404s and 139 are 403s, i.e. models
guessing URLs. websearch was called 11 times total in the same
store. The machinery is fine; the steering is absent.

## Requirements

- webfetch's description says guessed URLs mostly 404 and to find
  real ones with websearch first; websearch's description says it
  exists to locate pages for webfetch.
- The 404/403 error texts nudge: "the URL may be guessed wrong —
  websearch for the page instead of retrying variants".
- No behavioral machinery (no auto-search fallback) — steering
  only, measure again later.

## Acceptance Criteria

- Description/error wording tests updated; the texts contain the
  steering.

## Milestone

13 — Guard rails

## Outcome

webfetch's description says guessed URLs mostly 404 and routes
URL-finding through websearch; websearch says it exists to feed
webfetch; 404/403 errors append the nudge (other statuses
untouched). No machinery — steering only, to be re-measured with
the store-scan method after real use.
