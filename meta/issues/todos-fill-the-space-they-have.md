# Todos fill the space they have

## Summary

The todo sidebar shows at most five items (`TODO_SIDEBAR_MAX_ITEMS`)
no matter how tall the terminal is, and the leftovers collapse into a
muted `+N hidden` with no way to see them: the transcript deliberately
omits the current list, and a narrow terminal shows exactly one todo.
A twelve-item plan in a forty-row sidebar reads as "five items and a
number", which is worse than useless when the space is right there.

Two problems, one theme: the cap is arbitrary, and hiding is a dead
end.

## Requirements

- The sidebar shows as many todos as the panel has rows; the fixed
  five-item cap goes away.
- When the list genuinely does not fit, the item that matters stays
  visible: work in progress, else the next pending item, else the most
  recent completion. Trimming drops finished work above it before it
  drops upcoming work below it, and the run stays contiguous — a
  scattered pick of items 1, 2 and 17 reads as a list with silent gaps.
- The remainder is still reported as `+N hidden`.
- A modal shows the whole list, scrollable, from anywhere: a key
  binding, a palette entry, and a help entry. Read-only — the model
  owns the todos.

## Acceptance Criteria

- A twelve-item list in a tall sidebar renders all twelve and no
  hidden count.
- A test pins the active item staying visible when the list is taller
  than the panel, and the hidden count covering exactly the rest.
- A test pins the overlay listing every item including ones the
  sidebar hid.
- The full suite passes.

## Outcome

Two commits. The sidebar cap is now the panel's rows, and the visible
todos are a contiguous run anchored on the active item so the bigger
cap could not push it off screen once wrapping ate the rows — the old
scattered pick could show items 1, 2 and 17 as if consecutive. Ctrl-T
and a palette entry open a read-only scrollable overlay over the whole
list, titled with the done count, and the hidden note names the key
when the panel is at least 30 cells wide. Sidebar and overlay share
`todo_item_lines`. Smoke-ran in tmux against a synthetic twelve-item
session at 40 and 16 rows.

## Notes

- The wrapping trim already in `render_todo_sidebar_snapshot` stays:
  items wrap, so rows-available is not items-available.

## Milestone

10 — Everyday polish
