# Navigate slash completions with arrow keys

## Summary

The inline completion popup shown while typing a slash command cannot currently be navigated because prompt history and transcript scrolling consume the arrow keys first.

## Requirements

- Route Up and Down to the visible slash-completion popup before prompt history or transcript scrolling.
- Preserve existing slash-completion selection wrapping and acceptance behavior.
- Preserve normal arrow-key behavior when the slash-completion popup is not visible.

## Acceptance Criteria

- Pressing Down while slash completions are visible selects the next candidate.
- Pressing Up while slash completions are visible selects the previous candidate.
- Arrow-key navigation wraps at both ends of the candidate list.
- Automated tests cover slash-completion arrow-key routing.
