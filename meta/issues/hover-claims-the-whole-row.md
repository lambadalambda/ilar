# Hover claims the whole row

## Summary

An agent row is two lines and one click target, but only the
hovered line underlines; and `sidebar::underline_row` underlines
markers and indent, where the transcript's underline deliberately
skips structural spans. The surfaces disagree on what clickable
looks like. Underline all lines of the hovered target, content
spans only. Folds naturally into [[one-hit-map-for-the-sidebar]].

Size: S. Source: sweep 2026-08-29, rendering.
