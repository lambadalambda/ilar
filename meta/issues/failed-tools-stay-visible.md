# Failed tools stay visible

## Summary

- A collapsed tool group keeps only expanded or active rows
  (transcript.rs:1543-1546) and `tool_is_active` excludes `Failed`
  (1402-1413): the header reads `tools ▸ 3 calls · 1 failed ×` and
  the failed call is gone until the group is opened. The failed row
  itself (2538-2543) shows only `×` and the arguments; the error's
  first line needs a second click. Failed rows stay visible
  collapsed, and show the error's first line after the args.
- `write` cannot tell create from overwrite: "wrote path (N bytes)"
  (write.rs:104) and the row diff paints every line `+` on a rewrite
  (diff.rs:136). "overwrote path (N bytes, was M)" and a rewrite
  label.
- The `question` call row reads `question ▶ questions=2 items`
  (agent/turn.rs:1085-1100) where task shows its description. Show
  the first prompt.

Size: S. Source: UX sweep 2026-09-15, tools.
