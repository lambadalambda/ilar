# Pastes into pickers vanish

## Summary

`paste_target` (decide.rs:100) maps every modal except
CommandPalette/Search/Question to `PasteTarget::Discard` — but
`SessionSearch` is literally a typed grep query, and all the pickers
accept typed filter characters. Pasting a search term into
`/sessions` (or a filter into any picker) silently does nothing.

## Requirements

- Paste routes into the query/filter of whichever modal accepts
  typed characters; modals with no text field may keep discarding,
  but not silently claim the paste.

## Acceptance Criteria

- A test: paste with the session search open appends to its query;
  paste with a filter-less modal open leaves state unchanged.

## Milestone

12 — Health sweep

## Outcome

`PasteTarget::ModalQuery` routes pastes into the query of the six
filterable pickers and the session search (which restarts its
scan); the discard arm is spelled exhaustively so a future modal
must declare its paste fate at compile time. A shared
`insert_query` drops control characters, caps queries at 256 chars
(a runaway clipboard would stall the per-render fuzzy scorer), and
treats no-growth pastes as inert; the transcript search
(`PasteSearch`) shares the same policy. (60de4ac, 5d7e70a)
