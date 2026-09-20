# The replay sweep

## Summary

A survey for siblings of fc625c6 (a row rendering differently
depending on which path built it) turned up a family of them across
live/replay and TUI/web. Ordered by whether information is hidden.

## Requirements

### Hidden information

- **A restored `task_message` row never draws its children.**
  `session_view.rs:264` gives `ToolKind::Agent` only to `task`,
  while `restore_child_activity` is name-agnostic and fills
  `child_lines` for any row with a `child_session_id` — which
  `task_message` always sets. The rows are loaded and never drawn.
  Worse: a resumed subagent's `task` slice ends at the resume's
  `SubagentInvocation`, so the remainder lives only under the
  `task_message` row, and **after a restart the second half of a
  resumed subagent's conversation is invisible**. Fix mirrors the
  live one: promote any row `restore_child_activity` gave children.
- **A large `edit` diffs on replay and shows raw JSON live.** The
  live path diffs `tool_argument_detail`, already capped at 16 KiB,
  so past the cap it is not valid JSON and `tool_diff` yields
  nothing; replay diffs the raw input with far larger caps. Diff
  before bounding, or carry the raw input.
- **A killed process spins forever on the web.** Replay and the
  live TUI both sweep unfinished rows to `Failed`; the projection
  never sends a state, and the page's only rule is "no result → is
  running". Reopening a killed session shows an animated `running…`
  beside a session pill reading `idle`.
- **A running subagent's work is unreachable on the web**: the
  child fetch is gated on `result.child_session_id`, which only
  exists once the task finishes. The TUI shows a live preview
  throughout; the web shows a spinner for twenty minutes.
- **A notification routed to another session runs blind.**
  `subagent.rs:1584` hands `route_notification` a discarded event
  sender, so a whole turn — tools, edits — happens with the UI
  showing only "routing task to <id>". It also carries no
  `call_id`, so no `SubagentInvocation` is written and replay
  attributes its events to whichever invocation happened to be
  last.
- **A tool result over 16 KiB is permanently unreadable in the
  TUI** (the bounded copy is the only one kept) while the web
  serves the whole thing from its results route. Inverted
  direction from every other item here.
- **Task/job notification envelopes render raw on the web**, XML
  and all, attributed to "you"; the TUI unwraps them into a
  headline with the body behind a click.
- **A child transcript over 200 events silently loses its head on
  the web**: the page ignores `has_more` inside a child, and there
  is no "load earlier" there.

### Incomplete name-based special cases

- `summarize_tool_input`'s fallthrough takes the first three keys
  *alphabetically*: `service` loses `name` (which service?),
  `task_message` buries `task_id` behind a long message, `bash`
  drops `run_in_background`, and `todo`/`tasks`/`models`/`question`
  summarise to nothing.
- Only the tool literally named `edit` gets a diff; `write` shows
  its whole file body as escaped JSON, and the web computes no
  diff at all, so even `edit` is a wall of JSON there.
- The streaming argument preview is `write`-only although its
  `path` scan matches `edit` too, so a large streaming edit shows
  an empty argument column throughout.
- Progress labels and web glyphs miss most tools; the glyph map
  still lists seven tools this repo never had.

### Cosmetic drift

- A child that compacts mid-run shows nothing live and a compaction
  line on replay; `model reverted to X` live vs `switched to X` on
  replay; a UI-spawned subtask's "started" line is never persisted,
  and its activity is dropped outright because its `parent_call_id`
  is empty; assistant markdown is left as literal source on the web
  (no headings, lists or tables).

## Acceptance Criteria

- Each fixed item has a test pinning the two paths agreeing.
  Items deliberately left are listed in the outcome with why.

## Milestone

13 — Guard rails

## Progress (verified 2026-08-29)

Fixed, with evidence: restored task_message children (promotion in
session_view + test), large-edit diffs (unbounded ToolInputComplete
arguments), killed-process state on the web, running subagent
reachable via the invocations route, notification envelopes and
child paging on the web, summarize_tool_input's explicit arms and
IDENTIFYING_KEYS, web write/edit diffs, web markdown (tables
deliberately excepted), and the routed notification's call_id.

Still open, in rough order of value:
- The routed delivery still runs with a discarded event sender —
  visibility now comes from the registry row, not the stream.
- A tool result over 16 KiB is permanently unreadable in the TUI
  while the web serves it whole.
- TUI write diffs (web has them; diff.rs is still edit-only) and
  the streaming-preview write-gate.
- Web glyph map lists seven tools this repo never had and misses
  seven it has; no per-tool progress labels.
- Cosmetic drift: child compaction invisible live, "reverted
  to"/"switched to" wording, UI-spawned subtask's missing started
  line.

## Progress update (2026-08-29, wave 1)

Also landed since the first verification: TUI `write` diffs (pure
additions, real diff with old content — parity with the web), the
streaming argument preview covers `edit`, and replayed/projected
tool results are redacted like live ones everywhere. Remaining:
the routed delivery's discarded event sender, the TUI's 16 KiB
result cap, the web glyph map and per-tool progress labels, and
the three cosmetic-drift items.

## Progress update (2026-08-29, wave 2)

The TUI keeps tool results to 256 KiB behind the full toggle
(redacted copy; live rows still bound at the 16 KiB publish cap —
lifting that in turn.rs is the one remaining piece of this item),
a child's mid-run compaction shows live, and the web glyph map
matches the real tool set. Remaining: the routed delivery's
discarded event sender, the live publish bound, "reverted
to"/"switched to" wording, and the UI-spawned subtask's started
line.


## Progress update (2026-09-20)

Two more, and one struck.

**The 16 KiB live publish bound is gone.** A result was published cut
at `MAX_DETAIL_CHARS` while a reopened session read the whole thing
back from the log and kept 256 KiB, so the same call said two
different things depending on when you looked. `ilar::text` names the
result cap once as `MAX_RESULT_CHARS`, the publish sites use it, and
the TUI's own constant points at it. `MAX_DETAIL_CHARS` keeps its real
job — a tool's *arguments*, where 16 KiB is enough for a whole `edit`
to render as a diff.

The test that pinned the old divergence now pins the agreement; it is
the same test, since it also covers tab expansion and the image
markers riding past the cut.

**"model reverted to X" is "switched to X".** The live line and the
restored one described the same event in different words, and the live
one also dropped the `@variant` that the other two sites append — so a
session read back said something the live row had not.

**The routed delivery's discarded event sender is struck.** The
issue's own progress note answered it: visibility comes from the
registry row rather than the stream, and the `call_id` half — the part
that actually corrupted replay, by attributing the delivery's events
to whichever invocation happened to be last — was fixed. Giving it a
real publisher now would add a second source of truth for the same
rows.

Remaining: the web glyph map's per-tool progress labels and the
UI-spawned subtask's missing started line, which needs a synthetic
`call_id` rather than a new log variant.

## Progress (2026-09-20, later)

**A user-started subtask has a row now.** It had no tool call behind
it, so `ToolContext::call_id` was `None`, `parent_call_id` came out
empty, and `push_subagent_activity` dropped every event it produced —
by design, since an empty id can never attach and buffering it would
crowd the retry queue. A `/command` subtask therefore showed a system
line and then nothing at all, where a model-spawned one nests its
whole child timeline.

It gets a synthetic call id and the same agent row: `push_tool_row` +
`configure_subagent_row` on the way in, `finish_tool_row` with the
task tool's own "started in the background" on the way out, exactly as
a detached model-spawned task settles. A second benefit fell out of
it: the *child's* `SubagentInvocation` now records a distinct parent
id rather than an empty one, so two user-started subtasks in a session
no longer collide when a nested view looks its slice up.

**The persistence half is not this.** Nothing about a user-started
subtask reaches the *parent's* log: `SubagentInvocation` goes into the
child's. Recording it in the parent needs a new event variant, or an
assistant message carrying a `ToolCall` block — which would be a lie,
since no model called it. That is a log-format decision, not a fix to
slot in here.

Remaining: the web glyph map's per-tool progress labels, which is
parked with serve.
