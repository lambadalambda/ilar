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

## Outcome (2026-09-20)

The TUI half, which is the half that is not parked.

**One constructor.** `new_seeded_tool_row` with a `ToolSeed` that has
`Default` is where a tool row is born. The restore path spelled the
seventeen-field variant out itself with four fields filled in and
thirteen copied; it passes a seed now, and the defaults are shared by
construction rather than by everyone remembering them. The literals
that remain are test fixtures wanting a specific settled state, which
is a different thing from minting a fresh row.

**One settler.** The restore path had `finish_tool_row`'s rules written
out a second time, and the copy had drifted twice: it took the newest
row with a matching id whatever its state, where the live path refuses
one that has already finished, and it never cleared the progress. It
calls `finish_tool_row` now, handing over the redacted content and the
image markers whole — the one thing a replay really does differently,
because the log keeps raw values by design.

Pinned by `a_restored_row_settles_exactly_as_the_live_one_does`, next
to the parity test that already compared the two paths' result strings.

**serve is left, and is parked with the feature.** `serve/view.rs` is
behind an off-by-default Cargo feature; folding it belongs with the
rest of the serve work rather than ahead of it.
