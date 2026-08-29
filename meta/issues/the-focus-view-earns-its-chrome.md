# The focus view earns its chrome

## Summary

`render_focus` draws no activity row, no scrollbar, no tail/percent
title fragment — a stalled child and a scrolled-up view look
identical on the surface built for watching live agents. And the
slash-completion popup is gated only on modals, so a leftover `/…`
input pops "Tab/↵ complete" over a focus view whose keys route
elsewhere (view.rs:1013-1018).

Size: S-M. Source: sweep 2026-08-29, rendering.
