# Limits are not optional

## Summary

The blanket `impl ProviderResolver for T: Provider` answers None
for every limit — passing a bare provider silently disables
compaction and the session grows until the provider rejects it.
One type parameter away for every new embedding surface. An
explicit adapter that requires limits (or a loud warning when all
resolve None) fences it.

Size: S. Source: sweep 2026-08-29, core loop.
