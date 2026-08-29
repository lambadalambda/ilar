# Serve folds once and caches

## Summary

Every GET re-parses the whole log and re-sweeps the whole view,
under a page that polls listing/children/tasks every 3-5s — an
open tab on a big tree re-parses megabytes per second. The watcher
already tails every subscribed session; a folded-view cache keyed
on line number is the natural fix, and the first thing that breaks
at scale without it.

Size: L. Source: sweep 2026-08-29, serve.
