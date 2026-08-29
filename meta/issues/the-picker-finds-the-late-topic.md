# The picker finds the late topic

## Summary

`read_head` scans 40 events / 256 KiB; the Topic event lands after
the first turn — any tool-heavy first turn pushes it past the scan
and the picker shows the raw opening prompt forever. Scan for Topic
from the tail, or surface it in head-visible metadata.

Size: S. Source: sweep 2026-08-29, store.
