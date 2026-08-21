# Colour hierarchy and a theme pack

## Summary

Side by side with opencode and Claude Code, ilar's transcript reads as
louder and flatter. Three causes, all downstream of one design choice —
`theme::apply` remaps cells by ANSI colour name after render:

- **Every theme has exactly one background.** The only rule is
  `Reset → canvas`, so nothing can be grouped by surface. Both reference
  clients group with surfaces first and hue second.
- **With no surfaces, emphasis had to be `REVERSED`.** That is the white
  slab on every inline code span and every search hit — brighter than
  anything else on screen, applied to the most common inline element
  there is.
- **Syntax colours *are* the status colours** (keyword = MARKUP,
  string = SUCCESS, number = REASONING), so code speaks the same
  vocabulary as the UI and nothing in it reads as code.

On top of that the hierarchy is inverted: the most repeated rows carry
the most saturated colour. `Thought:` is LightMagenta on every reasoning
row; `tools ▸ N calls ✓` is green under each one. Green that appears on
every row cannot also mean success, and a real failure has to compete
with it.

## Requirements

- Surfaces: the palette carries `surface`, `surface_alt`, `code_bg`,
  `diff_add_bg`, `diff_del_bg`, `selection_bg` alongside the foregrounds.
- No `REVERSED` for inline code or search hits; both become tints. Mouse
  drag-selection may keep it — it is transient and terminal-conventional.
- Repeated chrome (thought labels, tool summary rows, tree glyphs) is
  muted; hue is spent on what is rare or eventful.
- Hues are reserved: cyan for live/interactive, yellow for waiting, red
  for errors, one accent for identity. Not four hues at equal weight.
- Code fences get their own syntax slots per theme, independent of the
  status colours.
- A pack of the well-known palettes: Monokai, Dracula, Gruvbox (dark and
  light), Solarized (dark and light), Tokyo Night, Catppuccin Mocha, One
  Dark, Rosé Pine.
- A tuned dark theme becomes the default; `terminal` stays available for
  people who want their own terminal colours.

## Acceptance Criteria

- Legibility is a test, not a judgement call: for every theme, body text
  against canvas and against each surface clears a contrast threshold,
  and every surface is a tint (close to canvas) rather than a slab.
- Inline code and search hits render with no `REVERSED` modifier.
- The adaptive `terminal` theme still works: it has no RGB surfaces to
  offer, so surface roles fall back to something that reads on both light
  and dark canvases rather than painting a dark block on a light one.
- The picker can find a theme by typing once the list is this long.
- The full suite passes.

## Outcome

Closed. Surfaces and syntax classes are palette slots reached through
`Color::Indexed` sentinels, so widgets still never see a theme. Inline
code and search hits tint; reasoning rows keep the hue on the label and
give the title to normal text; a tool group where nothing failed is
muted; diff rows carry their tint to the margin. Ten ported palettes
ship alongside the five authored ones, `carbon` is the default, and F3
filters as you type.

Legibility is enforced by test — AA on every canvas and surface, AAA for
the default theme, surfaces within 2:1 of their canvas, syntax classes
distinct and readable on the code surface. That is what made ten hand-
entered palettes tractable: the tints and any too-dim comment colour are
derived and then *checked*, rather than eyeballed.

Deliberately not done, because each needs the render width that the
markdown layer does not have until after wrapping:

- **Code fences have no block background.** They keep the `│` gutter and
  now-proper syntax colours; the surface exists in every palette and is
  ready when the wrap seam can pad to the column.
- **Tool output blocks are not on `surface`.** Same reason. These two are
  the remaining visible gap against opencode's look.

## Notes

- The ANSI-name keyed remap has run out of names. New roles use
  `Color::Indexed` sentinels — a namespace nothing else in the TUI uses.
- Render code stays theme-agnostic: it emits role sentinels, and `apply`
  resolves them. Per-theme branching at render time would undo the whole
  point of the layer.

## Milestone

7 — Unscheduled
