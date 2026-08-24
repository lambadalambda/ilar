# Sessions

Every conversation is an append-only JSONL file under
`${ILAR_STATE_DIR:-~/.local/state/ilar}/sessions/` — human-readable,
crash-safe, resumable, and never rewritten. Everything below builds on
that one property.

## Resuming and finding sessions

`ilar --continue` resumes the latest session from the CLI. Inside the
TUI, `/sessions` opens a two-pane search that is both the picker
(empty query lists sessions newest-first by topic, age and last words)
and a full-content grep (type to match against every session's
complete history, including material compaction has summarized away).
See [the interface guide](interface.md#switching-sessions-sessions).

Sessions name themselves: after the first completed turn a short topic
is generated and shown in the title bar, the listing, and the terminal
window title.

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
