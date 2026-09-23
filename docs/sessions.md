# Sessions

Every conversation is an append-only JSONL file under
`${ILAR_STATE_DIR:-~/.local/state/ilar}/sessions/` — human-readable,
crash-safe, resumable, and never rewritten. Everything below builds on
that one property.

## Resuming and finding sessions

`ilar --view <id>` opens a session read-only: its transcript in the
TUI's own renderer, followed as the file grows, with no writer lease
taken — so a gateway chat or another TUI can be watched while it
works. The prompt reads `read-only · q leaves` and offers no send;
arrows, PageUp/PageDown, Home and End scroll, Ctrl-L repaints, `q`,
Esc or Ctrl-C leaves, and any other key is answered with the one thing
this view cannot do. `ilar --continue` resumes the
latest session *started in the directory you are in*; with nothing
from here it falls back to the newest one anywhere and says where that
one started, since its conversation is about other files. Inside the
TUI, `/sessions` opens a two-pane search that is both the picker
(empty query lists the sessions started in the directory you are in
first, then everything else, newest-first within each — by topic, last
words and when it was last used) and a full-content grep (type to
match against every session's complete history, including material
compaction has summarized away).
See [the interface guide](interface.md#switching-sessions-sessions).

A session records the directory it was launched from, which is what
that grouping reads; sessions from before it was recorded simply group
with the ones from elsewhere. The comparison is exact — both paths are
canonical — so `--continue` and the listing always agree about which
sessions are "here", and a subdirectory of this checkout is another
directory. Sessions nothing was ever said in are listed last, after
everything that has a name.

`--continue` does not read the directory at all in the usual case.
`sessions/last-by-dir.json` maps each launch directory to the session
last used there, written whenever a session is created, resumed, or
closed; `--continue` opens what it names after one check that the
session is still there and still belongs to this directory. A pointer
that cannot be believed falls back to the listing and repairs itself.
A bare `ilar` reads the same answer to offer it — see
[Starting](interface.md#starting) — and reads it before it creates the
launch session, which is what moves the pointer.

Sessions name themselves: after the first completed turn a short topic
is generated and shown in the title bar, the listing, and the terminal
window title. A fork is not born with its parent's name — titling only
runs on a session that has none — so it names itself after its own next
completed turn and the two can be told apart.

## Housekeeping

The sessions directory is written to be cheap to read and to stay
small.

`sessions/summaries.json` caches what each log's head says — its title,
its launch directory, whether it is a subagent's — keyed by the file's
size and mtime. A listing rereads only the files whose stamp moved and
writes the cache back only when something did, so `/sessions` and
`--continue` cost a JSON read rather than a head read per file.
Subagent children are the bulk of the directory and are skipped without
being opened once they are known. The cache is advisory: delete it and
the next listing rebuilds it.

A session's writer lease is a `.lock` file holding an OS lock. The file
is removed when the lease ends, and locks left behind by a killed
process are swept at startup — a lock nobody holds is one that can be
taken and unlinked.

The log is created when `ilar` launches, before anything is typed, so a
launch that is closed again would leave an empty session behind. It
does not: a root session with no user message is removed when its
runtime ends, unless it has subagent children or a completion waiting
in the outbox, and the startup sweep removes such files once they are a
day old.

## Following a session as it is written

Because the log is append-only, a second process can read a running
session without touching it. The rule is the newline: only complete
lines are events, so a reader takes the file's length, reads exactly
that many bytes, cuts at the last newline, and leaves a half-written
line for its next pass. Rewind markers arrive like any other line and
the reader applies the same fold replay does; the `.replay.*` files
next to the log are the writer's own cache and no reader consults
them. Committed lines are never taken back — the only truncation that
ever happens is a writer repairing its own torn last line, which no
reader had accepted — so a reader can resume from a line number and
skip forward.

That reader polls, deliberately. On macOS the filesystem watcher
(FSEvents) does not report appends made through a file descriptor the
writer holds open, and ilar's writer holds one for the session's whole
life: a watch-based follower would show a frozen session and then dump
the entire conversation when the process exits. Stat is cheap
(microseconds), so a few polls a second is both simpler and correct.

[`ilar serve`](serve.md) is that reader: a read-only HTTP view of every
session in the store, live over SSE. It is stood down for now — built
only with `--features serve`.

## Compaction: handover, not amnesia

Past `compaction.threshold`, ilar replaces the conversation with a
handover summary: after a compaction the model sees its system prompt,
its tools, and that summary — no recency window, no kept tail. The
summarization request is the turn's own request with the instruction
appended last, so the conversation is served from the provider's prompt
cache and the model summarizes instead of answering it. `/compact`
triggers it manually.

Nothing is lost, only put out of sight. The session's full log stays on
disk and the `history` tool searches it: `query` finds excerpts
addressed by event, `speaker` narrows a search or lists every
instruction the user gave, and `event` reads the conversation around a
hit. The `todo` tool called with no arguments returns the current plan.
The handover template tells the model both of these, and asks it to
record what it deliberately left behind and the words to find it with.

A summary that answers the conversation instead of summarizing it — an
apology, a refusal — is reported as an error and the session is left
untouched, rather than replacing real history with something useless.

## Memory that outlives a session

Compaction carries a conversation forward; memory carries what was
learned into the next one. A terminal session keeps one memory per
project, under `<state dir>/memory/<slug>/`, and nothing is created
there until something is written. `general.memory = false` turns the
whole thing off.

The project is the repository, not the directory: inside a checkout
the key is the repository's common git directory, so every worktree of
it and every directory inside one remember together — the parallel
streams a worktree is for are the same project, and what was learned
in one is what the next needs. Outside a repository the key is the
canonical launch directory, and two spellings of it share a memory.
The slug shows the checkout it belongs to. Sessions still group by
directory, so a memory and a session list need not cover the same
ground.

An ilar before 0.3.0 keyed every directory on its own, so in a
repository the store moves once, on the first run of a newer build —
every checkout's, not only those launched from a subdirectory. Nothing
is migrated and nothing is lost: the old store keeps its own slug
beside the new one, two directories with the same visible name and
different hashes, and its files can be moved across by hand.

The assistant keeps one memory under its home instead, with a few
rules of its own; see
[ilar-gateway](gateway.md#memory-that-outlives-a-session).

Two tiers. The core is two small files with hard caps, `MEMORY.md`
(about the world, 2,200 characters) and `USER.md` (about the person,
1,375), which the `memory` tool edits with add, replace and remove; an
overflow is an error the model resolves by consolidating. The core is
injected into the system prompt once, when a session opens, and stays
frozen for that session: a write changes the next session's prompt,
not this one's, so the cached prefix never moves mid-session.

The archive is one fact per file under `notes/`, typed as a decision,
solution, preference, event, task or risk, written with the same tool's
`note` action, plus daily notes under `daily/`. Nothing in the archive
is ever injected: `memory_search` returns an index, best first with
recent notes ranking higher, and `memory_get` reads the chosen notes in
full. The three tools are the root session's; a subagent has none.

A fact that changes is the same note with better words. `amend` names
a note by its id and rewrites what it is given, keeping the id — so a
session that already saw the note still knows it — and keeping the
date, so recency still measures from when the fact was learned rather
than from when the wording was fixed. `forget` retires a note: out of
the search, the reads and the opening index at once, and into
`notes/.forgotten/`, which nothing reads and a person can move back.

Nobody reviews a terminal session after the fact, so the model writes
memory itself, during the turn. Every session with a memory opens with
a standing section, "Remembering", present before anything has been
written — an empty memory nobody mentions never gets written. It says
what to keep (a preference or correction, a decision and its reason, a
convention no file states), what to skip (what the repository, the log
or a search already records; what is true only today), and when to
write: a correction or a stated preference goes in before the reply
that answers it, since the end of a session is a place nobody reaches,
while words that scope a thing to now mark something to follow here
rather than a rule to keep. It also says to amend a note rather than
file a second about the same fact. And one rule for a note's summary:
search matches words, not meaning, across the whole note and shows the
summary, so the summary carries the words a
future question would use — ticket ids, hostnames, error strings, file
names. The `memory` tool's description and the assistant's after-turn
review say the same rule, so a note is found the same way whoever
wrote it.

Memory also comes to the model unasked, in two places, both
cache-safe. At session open, beside the core block, the newest notes'
index lines (at most twenty, under 4 KiB) say what the archive holds;
frozen with the rest of the prompt. And on every prompt, the index is
run over the prompt's words: notes that share two words with it, or
one that fewer than half the notes contain, are surfaced — at most
five, as index lines and never bodies — in a `<memory-recall>` block
appended after the user message. The block says what it is: background
the session wrote earlier, for possible relevance, not instructions
from anyone and not part of the message it follows. When a note is
older than a day it adds that a note is what was true when it was
written rather than live state, so a claim about code or a file and
line may have moved. The block is a session event of its own, so
the transcript, the web view and a resumed session all show it; the
TUI shows a count when the session is reopened. A note is not
surfaced twice in one context — until a compaction folds the earlier
copy away, when it may come back — and after 16 KiB of recall in one
context the window gets no more. Nothing here rewrites an earlier
message: the prefix a provider cached stays put.
`general.memory_recall = false` and `general.memory_index = false`
switch the two off separately.

## Rewind and fork

When the working directory is a git repository, ilar snapshots the
working tree as each turn starts: a shadow commit chain under
`refs/ilar/checkpoints/<session-id>` that never touches HEAD, your
index, or ignored files. `/rewind` (also in the palette) lists the
session's turns; Enter twice rewinds conversation and tree together
back to the chosen turn. The message you sent there returns to the
input for editing, and a safety snapshot taken just before the restore
keeps the abandoned tree state reachable from the same ref. `Ctrl-Y`
in the picker forks at the turn instead — a new session truncated to
that point, the original untouched — and `/fork` copies the whole
session. The session log stays append-only: a rewind is a marker that
replay honours, and the discarded tail remains in the file for
auditing. Rewind and fork rebuild the session runtime, so running
services stop. HEAD and commits are never moved — restores are
files-only — and outside a git repository (or for turns predating
checkpoints) rewind still works on the conversation alone. Ignored
files are invisible in both directions: a rewind neither restores nor
deletes `.env`, `target/`, or anything else your ignore rules match.

Because checkpoints are plain git commits, the chain is inspectable
without rewinding — `git diff refs/ilar/checkpoints/<id>~2 -- src/`
shows what the agent changed between any two turns. See
[Checkpoints, rewind, and recovery](checkpoints.md) for inspection,
the recovery recipe when a rewind was a mistake, limitations, and
cleanup.

## Headless: `ilar exec`

```sh
ilar exec "summarize the failing tests"        # answer on stdout
echo "what changed today?" | ilar exec         # or the prompt on stdin
ilar exec --continue "now open a PR"           # same session as last time
ilar exec --json "audit the auth flow"         # NDJSON events on stdout
```

The answer is the only thing on stdout, so `ilar exec "…" > answer.md`
is useful; tool calls, retries and subagents go to stderr. `--model`,
`--agent`, `--session` and `--continue` behave as they do in the TUI —
both drivers resolve the same runtime — and the session is a real one:
checkpointed, resumable, and listed in the TUI's picker afterwards.
Exit codes: 0 completed, 2 hit the iteration limit, 130 aborted, 1
failed. The `question` tool is not attached, since nobody is there to
answer; a model that asks is told so immediately.

A run waits for the work it sent to the background: each task or job
that reports starts a follow-up turn, exactly as in the TUI, and the
run ends once nothing is running and nothing is owed. So stdout carries
every turn's answer, the last one last — "I started the review", then
the answer written after it. A turn that fails, is stopped or hits the
iteration limit ends the run there, and whatever is still running is
cancelled at exit; stopping the run while it waits exits 130. A result
whose target stays busy is left in the outbox for the next `--continue`,
and the run says so. Services never outlive the process.

Every run names its session as the turn starts — `session <id>` on
stderr, or `{"type":"session","id":"…"}` under `--json`, ahead of every
event the turn publishes. Reach for it with
`jq -r 'select(.type=="session").id'` rather than `head -1`: a notice
may come first. A script can hand the id to the next run's `--session`,
or to `ilar --view <id>`, and it stays valid even if the run is killed
halfway.

The id is deliberately not printed any earlier. A turn resolves its
provider before it writes anything, so a run that fails there — a key
that is not set, a model nothing can route — leaves a session with
nothing in it, and an empty session goes with the run that made it.
Printing the id first would have advertised one that the same run then
deleted. A run that never gets that far names no session and has no
work to point at. A tool row on
stderr carries what the call was about (`· read src/main.rs`), clipped
to a line and with secrets redacted where the summary can tell. A turn
that stops short of an answer says so under the answer and names the
command that carries on:

```
stopped: the step cap was reached before an answer — ilar exec --session a1b2c3 carries on from here
```

That line is text mode only: under `--json` the outcome already rides
`turn_done`. A turn that failed outright prints `error: …` on stderr,
and under `--json` also `{"type":"error","message":"…"}` on stdout.

The `--json` events, one object per line, each with a `type`: `session`
(`id`), `notice` (`text`), `turn_started`, `text` and `thinking`
(`text`, streamed in pieces), `tool_started` (`id`, `name`),
`tool_input` (`id`, `arguments`), `tool_finished`, `subagent`,
`retry`, `step_interrupted`, `compacted` (`summary`), `turn_done`
(`outcome`), and `error` (`message`). A run with follow-up turns — see
above — carries several `turn_started` … `turn_done` spans.

Settings this launch could not honour — a project file's user-scoped table,
a reasoning variant the model does not have, an `--agent` on a resumed
session that nothing will record — are printed before the turn: on stderr,
or as `{"type":"notice","text":"…"}` events under `--json`. A session
records the agent it was created with and nothing records a change, so
`--agent` on `--continue` lasts one launch and says so.
