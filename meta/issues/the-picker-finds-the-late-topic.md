# The picker finds the late topic

## Summary

`read_head` scans 40 events / 256 KiB; the Topic event lands after
the first turn — any tool-heavy first turn pushes it past the scan
and the picker shows the raw opening prompt forever. Scan for Topic
from the tail, or surface it in head-visible metadata.

Size: S. Source: sweep 2026-08-29, store.

## Outcome (2026-09-05)

`read_head` now also reads the file's last 256 KiB and takes the newest
`Topic` line there, ahead of any topic the head scan saw and ahead of
the opening prompt: one seek and one bounded read per session, no
whole-log replay. A retitle later in the log wins over the first title.
Tested with a topic written after 120 tool results.
