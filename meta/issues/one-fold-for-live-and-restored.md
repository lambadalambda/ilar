# One fold for live and restored

## Summary

The restore path builds `Line_::Tool` as a 16-field literal and
re-implements `finish_tool_row`'s settle rules inline
(session_view.rs:287-371); serve's view.rs re-orchestrates the
same sweep decisions one layer up. This drift just minted
[[the-focus-view-settles-what-it-saw-running]]. Route restore and
serve through the shared constructors and settlers; a `ToolRow`
struct with `Default` (the 16-field variant is spelled out in 6+
places) rides along.

Size: M. Source: sweep 2026-08-29, rendering + serve.
