# An export stops at the last compaction

## Summary

Palette → Export writes `transcript_markdown(&app.session_id,
&app.lines)`. `app.lines` is the *display* window, and a restored
session's window begins at the newest compaction — `restored_session_
invocation_view` renders `events[cut..]`, where `cut` is the last
`Compaction`'s `kept_from`.

So exporting a session that was compacted and then reopened silently
drops everything before that compaction. The file looks complete; the
first half of the conversation is simply not in it.

The cut is right for the screen and right for the model: it is what
the model still carries, and the handover summary says the rest was
folded. It is wrong for an export, which is the conversation the person
had and wants to keep or share.

A session compacted *within the running process* is unaffected: a live
compaction sheds payloads but keeps its rows, so `app.lines` still
holds the whole session. The loss appears only after a reopen — which
is exactly when an export is most likely.

## Requirements

- An export carries every turn in the log, compacted-away ones
  included.
- A turn still running is not lost from the export either: the
  in-memory rows are the only copy of an uncommitted step.
- A compacted session's export says where the compaction fell, rather
  than silently splicing two windows together.
- The restore path's own cut is unchanged: the screen and the model
  keep the window they have.

## Acceptance Criteria

- A test compacts a session, reopens it, exports, and finds a
  pre-compaction turn in the markdown.
- A test exports mid-turn and finds the in-flight rows.
- Rendering the pre-window history costs nothing until an export is
  actually asked for.

## Notes

- Reported by the user, 2026-09-21.
- `store.load(id).events()` is not a reliable source on its own: on the
  indexed-replay path the reader's events are `[Meta] ++ window`, so
  the pre-compaction half is not there either. `store.audit_events` is
  every committed line.
- Rendering the pre-cut half at restore time would double the cost and
  the memory of opening a compacted session, which is the cost
  compaction exists to avoid. It has to be lazy.

## Done (2026-09-21)

The export splices: the folded half is rendered from the log and put in
front of the transcript as it stands.

Re-rendering the whole log instead — the first thing I tried — was
simpler and wrong twice over, which a review caught before it landed.
The on-screen rows are the only place a delegation's child timeline
lives (only the with-store restore fills `child_lines`, and only
`append_markdown` renders them), so a compacted session would have
exported with *every subagent's work missing* — trading one silent loss
for a worse one. And they are the only copy of a turn still running, or
of a message typed since the last commit.

Three readings of one log, now named as such on the store:

- `load` — rebased onto the active window. What the model carries.
- `audit_events` — every committed line, including the tails a rewind
  abandoned. What the file holds.
- `whole_events` — rewinds folded, no window. What *happened*, which is
  what an export wants. New, and the reason the first attempt would
  have resurrected turns the person had explicitly withdrawn.

`SessionReader::event_base` is the boundary between the two halves, and
`RestoredSessionView::history_before` carries it to the App. An earlier
draft used the compaction `cut` instead, which is almost always 1 —
the reader is already rebased, so the cut only steps over the `Meta`.
That number would have been a trap for the next reader.

The seam is marked for free: the window's own folded handover note
sits at the join, and the half in front of it folded nothing, so it
renders no note of its own. One "transcript compacted" in the file,
where the compaction actually fell.

Struck: the export does not refuse or warn while a turn runs. It does
not need to — the live rows come from the screen. It does say so when
the *folded* half could not be read, which is the one case where the
file is short of the conversation.
