# Hover claims the whole row

## Summary

An agent row is two lines and one click target, but only the
hovered line underlines; and `sidebar::underline_row` underlines
markers and indent, where the transcript's underline deliberately
skips structural spans. The surfaces disagree on what clickable
looks like. Underline all lines of the hovered target, content
spans only. Folds naturally into [[one-hit-map-for-the-sidebar]].

Size: S. Source: sweep 2026-08-29, rendering.

## Outcome (2026-09-20)

Both halves, folded into [[one-hit-map-for-the-sidebar]] as the issue
expected.

An agent is two lines and one target; the shared hover pass underlines
every line whose action matches the one under the pointer, so half a
clickable no longer lights up while the other half says it is
something else.

And the markers stay bare. The sidebar underlined `●`, `▸`, `✉` and
`⚙` where the transcript deliberately skips structure, so one
predicate now covers branch glyphs and row markers alike and both
surfaces mean the same thing by clickable.
