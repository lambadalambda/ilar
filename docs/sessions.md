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
answer; a model that asks is told so immediately. Background tasks and
services do not outlive the process.

Settings this launch could not honour — a project file's user-scoped table,
a reasoning variant the model does not have, an `--agent` on a resumed
session that nothing will record — are printed before the turn: on stderr,
or as `{"type":"notice","text":"…"}` events under `--json`. A session
records the agent it was created with and nothing records a change, so
`--agent` on `--continue` lasts one launch and says so.
