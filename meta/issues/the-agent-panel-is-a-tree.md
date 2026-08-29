# The agent panel is a tree

## Summary

The agents panel lists every running agent as a flat roster, which
turns a grandchild into a riddle: two agents live, one row showing,
because the second was a child of a child. The registry already
knows every task's `parent_session_id` — the panel just never uses
it. And with agent focus coming (see
[[a-clicked-agent-takes-the-screen]]), the panel becomes the map of
places you can go, so it needs a "main" row for the place you
started from.

## Requirements

- Agent rows indent under their parent when the parent is also in
  the panel; roots of foreign trees keep the existing "for {id}"
  note.
- A "main" row leads the panel — the root session, marked as the
  current place when no agent is focused.
- Every agent row (and "main") records a click hit-rect, plumbed
  like the disclosure row: this is the navigation surface focus
  builds on.
- The disclosure budget math counts the main row and survives
  indentation (truncation, narrow widths).

## Acceptance Criteria

- A child-of-a-child renders indented beneath its parent's row, not
  as an ambiguous sibling.
- The panel shows "main" first whenever it shows at all.
- Clicking an agent row is observable (hit recorded); actual
  navigation is the next issue's business.
- Panel math tests cover indent + main-row + disclosure together.

## Outcome

The panel leads with a "● main" row and indents every agent under
its listed parent — depths from a pure `tree_depths` over the
registry's parent edges (cycle-guarded, first-occurrence keyed,
order preserved). Indentation sits after the marker so ✉/▸ keep
column 0; the budget math counts the main row; an indented row
drops the "for {id}" note, since position now names the parent.
Every line of every row records where a click navigates
(`AgentTarget::{Main, Focus}`), hover underlines through the same
gate as the disclosures, and the hit plumbing is the disclosure
row's, shared.
