# Resume leads with this directory's last session

## Summary

Resuming should answer the common case with zero typing: "continue
where I left off *here*". Today the session pickers order by
recency across all directories and don't show when a session was
last used — the top row can be work from another project, and
telling near-identical sessions apart means recognizing their
titles.

## Requirements

- `SessionSummary` exposes the session's workspace (the meta is
  already read for the title; today the field is dropped).
- Both resume surfaces — the session search's empty state and the
  classic list picker — show a last-used timestamp per row:
  relative under a day ("2h ago"), date beyond that.
- With an empty query, sessions whose workspace matches the current
  working directory sort first (recency within), so the initially
  selected row is the last session used in this directory;
  other-directory sessions follow by recency, marked with their
  directory so the boundary is visible.
- Typing a query keeps today's behavior (match quality orders).

## Acceptance Criteria

- A picker test: two sessions, the newer from another directory —
  the top row is the older, current-directory one and shows its
  timestamp.
- A store test: `list()` summaries carry the workspace.

## Notes

- Sessions with no recorded workspace (older logs) sort with the
  "elsewhere" group.

## Milestone

12 — Health sweep

## Outcome

Two halves: d430e1b gave both resume surfaces last-used stamps
("35m ago" within a day, dates beyond) and the here-first ordering
machinery; 69190f6 made it live with a new informational
`SessionMeta.cwd` (canonicalized at root creation, byte-identical
to what the picker predicate compares; `workspace` keeps its
child-session meaning and the resume guard is untouched). Old logs
without the key group under "elsewhere"; other-directory rows carry
a `· ~/repos/other` marker. Pinned by render-to-buffer acceptance
tests on both surfaces plus store round-trips. Deferred (recorded
by the agent): >200-session scans truncate before the partition;
the id→cwd map could ride recall.SessionHits; a third open-coded
HOME lookup wants a shared helper.
