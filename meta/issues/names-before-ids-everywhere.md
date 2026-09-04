# Names before ids, everywhere

## Summary

[Mail says who](mail-says-who-and-stays-out-of-the-way.md) gave the
TUI a session resolver (`session_label`, main.rs) and used it for
deliveries. Every other place still shows a UUID: `"forked from
{session_id}"` (main.rs:2950), `"forked at that turn from …"` (3792),
`"deleted session {id}"` (3548), `"cannot delete/fork/resume {id}"`
(3554/3603/828); the roster's `for {id}` note on a foreign root
(2114-2117, which also truncates the elapsed time at narrow widths);
the search listing's untitled fallback `summary.id` (2444/2483) where
the classic picker says `(no messages yet)` (modals.rs:2097); the
focus title `agent · <id>` for a child off the roster (2125); the
export filename `ilar-transcript-{8 chars}.md` (app.rs:2298).

## Requirements

- Every user-facing session reference goes through the resolver or
  the picker row's own title; the short id appears only as a last
  resort, and never alone.
- Roster note: `for build · GM1 firmware dig`, middle-truncated.
- One shared placeholder for untitled sessions in both pickers.
- Export file named by topic slug when there is one.

## Acceptance Criteria

- `grep short_session_id` in ilar-tui finds only the resolver's own
  fallback and the export/id-display paths a test pins.

Size: S. Source: UX sweep 2026-09-03 (frame, overlays).

## Outcome (2026-09-04)

One head-based resolver (`session_name`, main.rs) under the cached,
roster-aware `session_label`. Named now: fork notices ("forked from
build · Add opencode providers…"), the picker's delete/cannot-delete/
cannot-fork notices (the name is read before the delete), the resume
refusal, the roster's `for …` note (middle-truncated to 24 columns so
the elapsed time keeps its place), the focus-view title for a child
off the roster, and the search listing's untitled fallback, which
now says `(no messages yet)` like the classic picker. The export file
is named by the topic slug when there is one. `short_session_id` is
called only inside the resolver's own fallbacks.
