# One hit map for the sidebar

## Summary

Three copies of click plumbing (services disclosure, agents
disclosure, agent rows), each with its own App field, hover block,
click method and per-frame reset. One `Vec<(Rect, SidebarAction)>`
with a single hover pass and one dispatch match collapses ~80
lines and makes the next clickable surface nearly free — which
[[the-agents-panel-reaches-its-tail]] will want.

Size: M. Source: sweep 2026-08-29, rendering.

## Outcome (2026-09-20)

`SidebarAction` names what a click means; `lay_out_hits` places the
rects and runs the hover once; `click_sidebar` dispatches. Three App
fields, three hover blocks, three click methods and three per-frame
resets became one of each, and the next clickable row is a variant
rather than a fourth copy.

The hover fix ([[hover-claims-the-whole-row]]) fell out of it, as the
issue said it would: once the map knows which lines belong to which
target, underlining all of them is where the loop already was.
