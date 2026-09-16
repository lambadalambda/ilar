# Sweep 2026-09-15 follow-ups

## Summary

What the stream reviewers found and the streams deferred, each too
small or too far from its issue to land in that batch. Omnibus; tick
items off here. The first three are the ones worth doing first.

- **A room seat can still read memory files.** Memory *tools* are
  withheld from non-private seats, but `read`/`bash` reach
  `<home>/memory/USER.md`. Containment, not just tool removal.
- **The situation stamp is frozen at session open.** A week-old seat
  carries a stale "now is …". A per-turn prefix at the end of the
  conversation is cache-safe and small.

Gateway:
- `/grant session <password>` leaves the sudo password in the chat
  with no deletion and no advice; `delete_inbound` is one call away.
  A typo'd `/unlok hunter2` falls to Unknown and keeps it too.
- `/abort` a second before shutdown gets the restart wording
  (`aborted_reply` reads the cancel flag at delivery time).
- `/abort` cannot cancel a `/compact` (compaction registers no
  `turn_cancel`); `/new` can.
- The dispatcher blocks head-of-line while a down channel eats
  4 × `send_retry_secs` per message; the ⚠ failure notice rides the
  same refused channel with one try; the `<delivery-failure>` note
  goes to the target chat's seat, not the sending seat.
- A turn killed by `abort_all()` at shutdown leaves its "working…"
  bubble; drain the `StatusBoard` next to `announce_stop`.
- The message tool awaits `clear_status` inside the tool call; a
  wedged rpc hangs the turn there.
- The gateway ignores `RuntimePlan::notices` (a dropped reasoning
  variant is announced nowhere there).
- `TurnError::Closed`'s guard has no dedicated test.

Secrets:
- A job backgrounded from a terminal still has `/dev/tty`, so the
  master-password prompt can block it (SIGTTIN).
- `App::secrets_locked` is a startup snapshot: a store resealed
  mid-session raises no notice.
- The first store file created mid-session: the parent still lacks
  the `secrets` tool the children get.
- The TUI grant transcript line says "(always)" when the store write
  failed (the tool result carries the note).
- The serve driver gets the generic unlock hint.

Tools:
- `write` cannot tell "new file" from "could not stat".
- `grep_one_file` records one clip cause per file.
- webfetch has no offset/range; the spill file is the answer.
- `format_duration` floors to seconds: a sub-second configured
  timeout renders `0s`.

TUI / session:
- `quit_warning` says "the running turn" for a compaction or a
  restore too.
- `store.latest_in` compares the recorded cwd as-is; a
  pre-canonicalisation log is "here" in neither picker nor
  `--continue`.
- `--view` still silently ignores the project-instruction flags.

Agents / delivery:
- `workspace.acquire_lease` in `route_notification` has no cap and
  runs while holding the session claim: a mutable task holding the
  lease reproduces the unbounded-wait symptom.
- Held results have headlines and deliver-all in Ctrl-Q, but no
  viewer for a body.
- `QUIET_MARKER_AFTER` (30 s) against a heartbeat touched by loop
  events: a provider that does not stream during a long reasoning
  step makes a healthy task wear `· quiet 45s`.

Structure:
- `Config::provider_for` has no non-test caller (thin wrapper kept
  for eight test call sites).
- `AFTER_HELP` prose duplicates `credential_sources`.
- The subagent-mark rule for grant prompts lives in both
  ilar-tui/src/grants.rs and ilar-gateway/src/grants.rs.

Size: S each. Source: stream reviews, UX sweep 2026-09-15.

From stream C:
- sidebar.rs still prints `job` in a ⚙ row\x27s agent-name slot (the
  count half of the panel-title item is done, the label half is not).
- docs/interface.md opens with "Press F1 any time", false under a
  grant prompt and a question modal, which outrank Help.
- docs/assets/sessions.svg still shows `↵ resume`; a generated
  screenshot to regenerate.

From stream T (delivery):
- serve/drive.rs hand-rolls the disposition instead of folding
  `delivery::disposition`: no retire when the hop budget runs out, no
  salvage on Err.
- The TUI\x27s Exhausted salvage writes only to the in-memory transcript;
  a quit loses the salvaged text.
From stream R (sessions):
- `file_may_contain` folds ASCII while `recall::search` folds Unicode.
- A `delete` that returns WouldBlock still leaves the directory pointer
  on the session just judged empty.
From stream O (the resume offer):
- A log over 4 MiB whose last line is a rewind gets no offer.
- Enter with text dismisses the offer even when nothing was sent (a
  refused submit); the ghost cannot be brought back.
