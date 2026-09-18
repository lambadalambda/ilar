# Notes can be amended and forgotten

## Summary

A note in the memory archive is write-once: the `memory` tool has
replace and remove for the two core files, but nothing edits or
deletes a note, and neither the gateway's after-turn review nor its
weekly review can retire one. Over months the archive accumulates
facts that later turned out wrong, and recall surfaces them beside
the ones that replaced them.

Claude Code's write guidance is "update the existing file rather than
creating a duplicate; delete memories that turn out to be wrong", and
its consolidation pass deletes contradicted facts and rewrites
relative dates as absolute ones (vault write-up, 2026-09-18). The
archive needs the same two verbs.

## Requirements

- `memory` gains `amend` (by id: any of title, summary, body, kind;
  `when` stays, so recency ranks by when the fact was learned) and
  `forget` (by id; the file is moved to `notes/.forgotten/`, not
  deleted, so a mistake is reversible by hand).
- The tool's description and the "Remembering" section say to check
  for a note that already covers a fact and amend it rather than
  write another, and to forget a note that turned out wrong.
- The after-turn review's plan can carry `amend` and `forget` entries
  for notes, applied like its core edits.
- The weekly review's prompt asks it to retire notes the week
  contradicted and to rewrite relative dates in note bodies as
  absolute ones.
- A forgotten note leaves the index, the search and the opening index
  at once; a session that recalled it earlier is not rewritten.

## Acceptance Criteria

- Tool tests: `amend` changes the summary and the next search ranks
  by the new words; `forget` removes the note from `memory_search`,
  `memory_get` and `opening_index`, and the file is under
  `.forgotten/`.
- A review test: a plan with a `forget` entry retires the note.
- The tool description and the section carry the update-not-duplicate
  and forget rules.
- Docs: sessions.md names the two verbs; gateway.md names the weekly
  review's two new duties.

## Notes

Follow-up to the memory stream (milestone 22), from the write side of
Claude Code's auto-memory as written up in the vault, 2026-09-18.

Size: M.
