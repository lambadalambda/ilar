# The big files find their seams

## Summary

Four files hold half the repo's growth risk, each with verified
clean seams: subagent.rs (3.2k — steering, routing, tool surfaces,
and ReservedNotification each extract; the 720-line
run_task_observed closure becomes a named fn), turn.rs (2.7k — the
JSON path scanner, the summary/redaction suite and the question
sub-flow are pure extractions), store.rs (1.8k — listing/head
scanning and the transcript renderer decouple; the two hand-rolled
window-cut remaps want one helper or a pinning test), modals.rs
(4.7k — an armed_row layout helper plus a directory split). App's
substructs (StreamStats, SearchState, HitMap, one modal enum) ride
the same wave. All mechanical; none should be done as drive-by
edits inside feature work.

Size: L, incremental. Source: sweep 2026-08-29, all territories.
