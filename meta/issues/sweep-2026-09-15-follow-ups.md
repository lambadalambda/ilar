# Sweep 2026-09-15 follow-ups

## Summary

What the stream reviewers found and the streams deferred, each too
small or too far from its issue to land in that batch. Omnibus; tick
items off here.

Done 2026-09-16: the three that led this list — a restored aborted
session that could not resume, a room seat that could read the memory
files its tools were denied, and the situation stamp frozen at session
open.

Done 2026-09-20: the password left in the chat. `/grant session <pw>`
already deleted and advised by the time this was read; the typo did
not. A command name within two edits of `unlock` or `password`, with an
argument behind it, is now `Command::MistypedSecret`: named as the typo
it is, and the message taken back out. The delete decision is
`Command::carries_a_secret` and one call site, rather than three arms
each doing their own — and matching a reply's text to decide whether to
delete it was the fragile part of the old shape.

Done 2026-09-20, in three batches:

*Gateway.* `/abort` a second before shutdown got the restart wording —
the reply read the gateway's own shutdown flag at delivery time, so
the person who asked for the turn to stop was told to send it again;
the seat records that the abort came from the chat. `/abort` could not
cancel a `/compact`, the one piece of work most worth stopping:
compaction registers a `turn_cancel` now. A turn killed by
`abort_all()` at shutdown left its "working…" bubble in the chat,
still there at the next start; `StatusBoard::clear_all` sweeps them.
The message tool awaited `clear_status` inside the tool call, so a
wedged rpc hung the turn — bounded at five seconds and logged.
`RuntimePlan::notices` went nowhere here and now reach the log.

*Tools.* `write` read a failed stat as a new file, so a creation was
reported over whatever it then replaced. `grep_one_file` kept one clip
cause, so a file that hit the byte cap on its way to the match cap
reported only the second. `format_duration` floored to seconds, so a
configured 400 ms timeout said `0s`.

*TUI and session.* A `delete` refused because another process holds
the writer lease read as "keep this session", and the directory was
left pointed at an empty log. Withholding the pointer turned out not to
be enough on its own — `create` writes it for every new root session —
so it is taken back by hand.

*Structure.* The subagent-mark rule for grant prompts lived in both
frontends, with the boolean the other way round; it is
`ilar::secrets::asker_label` now.

*More of the same, later the same day.* `store.latest_in` and
`last_in` resolved the directory being asked about and compared it
against whatever the shell had handed the session, so a log that
recorded `/tmp/x` was in neither the picker nor `--continue` when the
canonical path is `/private/tmp/x` — which every macOS temporary
directory is. The pointer file had the same split, written under the
unresolved path and read by the resolved one, so such a pointer was
one no read could ever find. `TurnError::Closed` has its test: a turn
queued behind the one `/new` cancelled never speaks.

Two struck rather than done. `file_may_contain` folding ASCII while
`recall::search` folds Unicode is already handled for the half that
matters: `greppable` refuses a non-ASCII needle outright, so a
non-ASCII query pays the full parse. What is left is a file containing
U+212A KELVIN SIGN searched for `k`, which the function's own doc
argues is the right trade — closing it means Unicode-folding every
chunk of every file, which is the cost the prefilter exists to avoid.
`App::secrets_locked` as a startup snapshot was fixed in `2e24d7d`.

Also done 2026-09-20: `quit_warning` offered to end "the running
turn" for a compaction and a restore alike, because `busy` cannot tell
them apart — the two statuses are named constants now and the warning
reads them (and counts in words rather than in `agent(s)`). `--view`
refuses the project-instruction flags rather than ignoring them, which
read as the flags not existing. The ⚙ row printed `job`, the internal
agent name for a cron turn, in the slot where every other row names
its agent — saying nothing twice, and now nothing at all. And
docs/interface.md no longer opens with "Press F1 any time", which a
grant prompt and a question both outrank.

Structure, 2026-09-20: `AFTER_HELP` listed the provider key variables
by hand beside `credential_sources`, which reads them from the
provider table — so adding a provider meant remembering two places and
the one nobody remembered was the help. It is generated from that
table now, grouped by variable because two providers share one.

Struck rather than done: the `· quiet 45s` marker against a heartbeat
touched by loop events. The heartbeat already *is* touched by every
event; what is left is a provider that emits none at all during a long
reasoning step, which no signal here can tell from a hang. The marker
is a threshold, and picking a new number is a judgement call rather
than a defect to fix.

Stream O, 2026-09-20: Enter dismissed the offer on the keypress, so a
submit that was then refused took the offer with it and nothing brings
one back. The send answers it now, at the site that knows whether the
send happened. Only reachable with a draft that arrived without
typing — a popped stash or a carried prefill — because a typed
character or a paste dismisses the offer on the spot.

More, 2026-09-20. A routed delivery waited on `acquire_lease` with no
cap while holding the session claim, so one mutable task that kept its
lease stalled every delivery behind it; the wait is bounded and the
notification requeued. The master-password prompt read the terminal
without asking who owned it — a job backgrounded from a shell keeps
`/dev/tty`, so the read raised SIGTTIN and the shell stopped the job
with no prompt on screen; the foreground process group is checked
first. The `secrets` tool was installed or not when the registry was
built, so the first `ilar secret set` of a machine's life left the
running session without the tool its own bash schema points at: tools
answer `is_published` for themselves now, and this one reads the store
file per turn. `Config::provider_for` is gone; its eight test call
sites ask `provider_result` directly.

Also 2026-09-20: the TUI's Exhausted and Salvage salvage wrote only to
the in-memory transcript while retiring the outbox entry on the spot,
so a quit lost the child's last word for good. It goes into this
session's log now, as the user message a delivered one would have
been — except where the log will not take it, which the message says
rather than glossing. The review of that change found three things
outside it, now their own issues: an append that can write a log no
later open can read, two consecutive user messages concatenated with
no separator on the wire, and a late arrival that takes an interrupted
turn's resume offer away.

Struck rather than done: the TUI grant line saying "(always)" when the
store write failed. The modal writes that line from the answer, and
the write happens in the broker on the far side of a one-way channel —
telling the line about the downgrade means a reply path back for a
case that is a disk failure. The note already reaches the person: it
rides the tool result, which the transcript shows.

Gateway:
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
