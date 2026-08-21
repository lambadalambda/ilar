# Send the prompt cache key on the ChatGPT backend too

## Summary

[Stabilize OpenAI prompt caching](stabilize-openai-prompt-caching.md) added
`prompt_cache_key` and deliberately withheld it from ChatGPT OAuth: a live
probe on 2026-08-19 found the Codex backend *accepted* the field but the
keyed samples reported 0/0/0 and 0/6912/0 cached tokens — no better than
the unkeyed control at 0/6912/0. The conclusion was that the field bought
nothing measurable, so the documented automatic behaviour was kept.

That evidence was inconclusive rather than negative: two samples of three
requests against a backend whose routing is the variable under test cannot
show an affinity effect either way. Meanwhile the symptom it was meant to
address is still visible in daily use — cache reads climb, drop by a block
or so at a turn boundary, then climb again — and Codex CLI sends a cache
key to the same endpoint.

The field is accepted, costs one JSON key, and is the only affinity lever
the API offers. Withholding it on the strength of a null result is the
wrong default.

## Requirements

- ChatGPT OAuth sends `prompt_cache_key` (the session id) by default.
- The rule is the same on both auth paths: the documented endpoint gets the
  field, a custom `base_url` does not, since a gateway may reject unknown
  fields.
- The live probe can still run both arms, so the experiment stays
  repeatable now that keyed is the default.

## Acceptance Criteria

- A unit test pins that the ChatGPT backend receives the key, replacing the
  test that pinned the opposite.
- A unit test pins that a custom `base_url` omits it on both auth paths.
- The full suite passes.

## Notes

- This does not claim the dips will stop. It removes the one variable we
  control; if reads still oscillate with a key attached, the remaining
  explanation is backend routing plus the 128-token reporting granularity,
  and there is nothing further to do locally.

## Milestone

7 — Unscheduled
