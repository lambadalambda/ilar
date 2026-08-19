# Prompt history recall

## Summary

Up/Down in the input only scrolls the transcript (single-line) or moves the
cursor (multiline). There is no way to recall a previous prompt.

## Requirements

- Up on an empty or unmodified single-line input recalls previous prompts;
  Down moves forward; editing a recalled prompt then pressing Up preserves
  the draft (readline-style stash).
- History persists across sessions in the state dir (bounded, e.g. last
  1000 prompts, deduplicating consecutive repeats).
- Multiline recalled prompts are handled (recall replaces the whole input).
- Keep transcript scrolling predictable: Up means history when the input is
  empty/unmodified; scrolling remains available via PgUp/wheel/Ctrl-U.

## Acceptance Criteria

- Unit tests for the history ring (recall order, draft stash, dedup, cap).
- History file survives restart and is shared across sessions.

## Notes

- Store as JSONL or plain lines with escaping for newlines.
