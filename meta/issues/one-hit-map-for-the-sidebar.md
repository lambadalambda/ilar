# One hit map for the sidebar

## Summary

Three copies of click plumbing (services disclosure, agents
disclosure, agent rows), each with its own App field, hover block,
click method and per-frame reset. One `Vec<(Rect, SidebarAction)>`
with a single hover pass and one dispatch match collapses ~80
lines and makes the next clickable surface nearly free — which
[[the-agents-panel-reaches-its-tail]] will want.

Size: M. Source: sweep 2026-08-29, rendering.
