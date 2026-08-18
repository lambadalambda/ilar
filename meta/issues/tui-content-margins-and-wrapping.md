# TUI content margins and wrapping

## Summary

The transcript currently reaches the sidebar border, wraps long words poorly, and renders sidebar todos too narrowly to scan. Match OpenCode's use of content margins and improve wrapping and todo readability across terminal sizes.

## Requirements

- Add two cells of horizontal breathing room around transcript content, including a visible right margin before the sidebar or terminal edge.
- Wrap prose at word boundaries when possible without clipping long unbroken content.
- Use a fixed 42-column todo sidebar above 120 terminal columns, with two cells of internal horizontal padding.
- Wrap todo text with continuation lines aligned after the status marker, preserve accurate hidden-item counts, and increase completed-item contrast.
- Preserve narrow-terminal behavior, scrolling, selection, and wide-character correctness.

## Acceptance Criteria

- Wide layouts visibly separate transcript text from the sidebar border.
- Normal prose wraps on words rather than splitting words at the content edge.
- Preformatted Markdown preserves significant whitespace when hard-wrapped.
- Sidebar todos remain readable, retain clear status markers when wrapped, and report items displaced by wrapping.
- Focused layout regressions and the full workspace checks pass.
