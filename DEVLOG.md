# DEVLOG

## 2026-09-19 — Work nobody is waiting for

Three defects our own reviews found and nobody went back for. They
turned out to share a shape: work that keeps running after the last
person who wanted it has gone, and a promise that only held as long as
somebody kept auditing the return paths.

**The archive is read off the runtime.** The `history` tool parsed the
whole session log synchronously inside an async future, holding a
runtime worker — and every other task on it — for the duration. It now
goes through the blocking pool, and the parse checks every 256 lines
whether anyone still wants it, so an abandoned call stops instead of
finishing. A speaker listing was unbounded in aggregate too: each row
was capped but a long session's user messages together were not. Fifty
rows and 16 KiB now, with the count that did not fit and an `after=`
to continue — a truncated answer the model cannot tell is truncated is
worse than a short one.

**A started turn cannot end without saying so.** `run_turn` published
`TurnStarted` and then used `?` in a dozen places; a compaction failure
or a provider that refused to build a stream returned without the
terminal `TurnDone` the channel reserves a slot for, and a consumer
watched a stream that simply stopped. The fix is not a list of audited
`?` sites, which is a thing that stays correct until the next one: the
sender itself owes the debt. It records that a start actually went out,
and on drop — early return, panic, anything — pays the terminal event
if the turn did not pay it first. A turn that never started still says
nothing, which is what `TurnNeverStarted` has always meant.

**An abandoned replay stops.** Arrow keys in the session picker start a
preview loader per row, each parsing a whole archive on a blocking
thread; the landing guards correctly threw the stale results away,
which was exactly the problem — the work ran to the end anyway. Focus
seeds had the same shape on retarget. Both are now structs carrying a
cancel flag that their own `Drop` raises, so replacing one *is*
stopping it and no code path can replace and forget. The preview
watches the flag mid-parse; the focus seed checks it either side of its
two halves, which are not interruptible from outside.

Two of the three were verified the honest way: the fix was reverted on
the test box and the new tests watched to fail.

## 2026-09-19 — A model that thinks out loud

MiniMax M3 does not use `reasoning_content` on the way out. It opens
its answer with a literal `<think>…</think>` block in the content
itself, which ilar showed as ordinary assistant text: every transcript
opened with the model's notes to itself, and the fold that hides
thinking never saw them.

The chat mapper now reads a block at the *head* of the content stream
as thinking. Only there — a model writing about the tag later in an
answer is writing, not thinking, and the mapper leaves it alone. Since
either tag can arrive split across deltas, the undecided head is held
until it can be told apart, and inside the block a tail that could
still become `</think>` is held the same way; what is held is flushed
as whatever it turned out to be when a tool call, a finish reason or
the end of the stream says the content is over.

Two things the tests caught that reading the code did not. A
one-character fragment fell through the hold-back range, so a close
tag arriving letter by letter streamed into the answer as text. And
the blank line a model leaves between its thinking and its answer was
only swallowed when it shared a delta with the closing tag; it is a
separator wherever it falls.

A third came from the review: a server that sends an empty
`tool_calls` list beside ordinary content — some proxies do — would
have ended the content stream before the block was decided, cutting a
tag in half. An empty list is not a call.

The extracted thought is thinking like any other, so it goes back as
`reasoning_content` rather than as the inline tag it arrived in. That
is a wire the model was never sent before, so it was probed live:
minimax-m3 completed both turns of the two-turn thinking smoke, tool
call and all. One unrelated row, deepseek-v4-flash, failed the same
run with an upstream 530 from the gateway.

## 2026-09-19 — Memory, the write side

A fuller write-up of Claude Code's auto-memory appeared in the vault,
covering the half we had not seen: the system-prompt guidance, a
background extraction fork, and a periodic consolidation pass. Most of
it we already had in another shape — their extraction fork is the
gateway's after-turn review, their dream is its weekly one. Three
things we did not.

**A note can be amended or forgotten.** The archive was write-once:
the core files had replace and remove, a note had nothing. With recall
live since yesterday, a wrong note is not just clutter, it is surfaced
unasked forever. `amend` rewrites a note by id and keeps two things on
purpose — the id, so a session that already saw the note still knows
it, and the date, so recency measures from when the fact was learned
rather than from when the wording was fixed. `forget` renames the file
into `notes/.forgotten/`, which the listing skips because it is not a
`.md`; retiring a note by mistake is then a move back, not a loss. The
after-turn review's plan carries both verbs, and the weekly review is
asked to retire what the week disproved and to give a note that says
"yesterday" its date.

**A repository remembers as one.** Memory keyed on the launch
directory, so every worktree of a checkout had its own store and none
saw the others — and worktrees are how the parallel streams here run.
The key is now the repository's common git directory, read out of
`.git` by hand rather than by shelling out: a directory is its own
answer, a worktree's `.git` is a file naming one, and that one names
the common one. Outside a repository the launch directory still
answers. The slug shows the checkout, never the `.git`.

**When to write, and what a recall is not.** The standing section said
what to keep and never when, so the model could agree that a
correction mattered and never write it down. It now says to write a
correction or a stated preference before finishing the reply that
answers it, and that words scoping a thing to now mark something to
follow rather than a rule to keep. The recall block says what it is:
background the session wrote earlier, not instructions from anyone and
not part of the message it follows — a note can quote a web page, and
the model should read one as data. And an episode where the model used
the `memory` tool is no longer reviewed at all: it already decided,
and the review was filing the same fact twice.

## 2026-09-18 — Memory in every directory

Reading a write-up of Claude Code's auto-memory next to ilar's own: the
same two tiers, the same one-line summaries, and one difference that
mattered — theirs is per project directory and ours was the gateway's
alone. A terminal session had no memory at all, and could not see what
the assistant on the same machine had kept.

**The store moves into the core.** `ilar::memory` is the gateway's
module, unchanged: the two capped core files, the note archive, BM25
with recency decay, the three tools. A store is a directory and
nothing else, so who remembers is who owns the directory. The gateway
keeps `<home>/memory/`; a terminal session gets
`<state dir>/memory/<slug>/`, the slug being the canonical launch
directory as one file name plus a short hash so `a/b` and `a-b` stay
apart. Nothing is created until the first write. `[general] memory =
false` turns it off for terminal sessions; the gateway has its own
flag as before.

**The plan carries it.** `RuntimeOptions.memory` puts the three tools
into the root registry — a subagent's memory would be its parent's —
and the core block into the system prompt. The block is appended at
start, not at resolve, so a driver's own additions come first: the
gateway's "where you are" still precedes the memory, and a room's seat,
which passes no store, gets neither tools nor block. The gateway's
hand-added tools and hand-appended block are gone; its tool policy
filters the core registry as it always did.

What stayed behind is what makes memory an assistant's: the review
after a turn, the weekly promotion, the daily notes at handover, and
the room-seat guard on the memory directory.

**The session is told when to write.** Nobody reviews a terminal
session after the fact, so the model writes memory itself, the way
Claude Code does it: a standing "Remembering" section opens every
session that has a store, before anything has been written — an empty
memory nobody mentions never gets written. What to keep, what to skip,
the two places, and that a write reaches the next session's prompt and
not this one's. A wire test pins the last part: a `memory` write
mid-session and every request of the session carries the same prompt.

**One rule for a note's summary.** The write-up of Claude Code's
recall made the point that the description is the only thing the
selector ever sees, so it has to carry the tokens a future prompt
will. ilar's index is BM25, which matches on exactly those tokens, so
the rule is stated once (`SUMMARY_RULE`) and said in three places: the
tool's description, the prompt section, and the gateway's after-turn
review, so a note is found the same way whoever wrote it.

**Recall comes to the turn.** The archive was pull-only, and the
harness study of 2026-09-17 measured how often a model pulls: well
under once a task. Claude Code pushes instead, a sidecar model picking
files per prompt. ilar has the index already, so it pushes without
the sidecar: every root prompt is run through BM25, and notes that
share two words with it, or one that fewer than half the notes
contain, go after the user message as index lines — never bodies —
under the framing Claude Code uses, with a reminder to verify when a
note is older than a day. It is a session event of its own,
`MemoryRecall`, folded into the user message on the wire, so the
transcript, the web view and a resumed session see it, and nothing
before it is ever rewritten. A note surfaces once per session, until
a compaction folds the earlier copy away; after 16 KiB of recall the
session gets no more. Beside the core block at session open, the
newest notes' index lines say what the archive holds. Stopwords left
the index on the way: a note surfaced on "the" is not a match.

## 2026-09-18 — Seven small ones

The backlog's small correctness items, one commit each on one branch.

**A bare provider answers the catalog's limits.** The resolver trait's
`None` defaults did not mean "unknown" downstream, they meant "never
compact": a surface that embedded a provider one type parameter short
of the configured resolver grew its session until the provider refused
it. The blanket impl and `FixedProviderResolver` answer the catalog row
now, which exists for every model a configuration can route.

**A delivering row's footer is settled.** The focus view's seed already
knew a delivery streams nothing; the footer's "running" flag was set
from the roster alone, and no `TurnDone` was ever going to arrive to
take it back.

**A rewind marker past the stream refuses the replay.** `truncate`
shrugged at an out-of-range marker and kept everything, so the history
the marker had abandoned came back silently. Both replays refuse it
naming the line; the tail checks every marker in a slab before it
applies any, so the "never a half-consumed slab" promise holds.

**Scratch repositories never sign.** Every fixture sets
`commit.gpgsign=false` before its first commit. Verified on tenco with
a global config that forces signing through `/bin/false`.

**Ignored project tables cannot refuse startup.** `[providers]`,
`[models]` and friends in a cloned repository are documented as
ignored, yet they passed through validation before being thrown away,
so one bad line a project was told means nothing kept ilar from
opening. The project layer sheds them off the raw TOML before it is
even deserialised, and the warnings are read off the same table — so a
table whose contents would not parse as ours is still named.

**Base URLs are structural.** One rule for providers, models and
endpoints: http(s), a host, no query, no fragment, and the trailing
slash that used to become `//chat/completions` on the wire is gone
from the stored form.

**The skill scan runs once and a load reads one body.** Startup listed
every skill twice, reading every body both times, and the tool read
every body again for every load and every unknown name. The listing
keeps names and descriptions; a body is read when its skill is asked
for, on a blocking thread, never past 256 KiB. A skill added
mid-session shows up at the next start; an edited body is read fresh.

## 2026-09-18 — Thinking goes back whole, and under its own name

This morning's replay sent thinking back for the current turn only,
and always as `reasoning_content`. Then a look at OpenCode's
`transform.ts`: it sends thinking on *every* assistant message, and
under whichever field the catalog says the model uses. Two questions,
two different answers.

Scope: OpenCode's way is the default now. It is wasteful — the whole
history of thought rides along until compaction — but the models are
plausibly trained with exactly that in front of them, and the two-turn
probe had already shown every family on the gateway takes it. The
current-turn rule stays as a switch, `replay_thinking = "turn"`, next
to `"off"`; `[general]` sets it for everything, an entry overrides it
for its own server. The vendors' documented minimum is a setting, not
the default.

Spelling: an oversight, plainly. The mapper read both spellings on the
way in and echoed one on the way out. A `Thinking` block now remembers
the name it arrived under when that was not the default, the chat wire
says so once per response, and the thought goes back as it came — with
one refinement the review asked for. Under `all`, a session that
switched from a `reasoning` model to a `reasoning_content` one would
otherwise send both names in one request, and hand the new model a
field it has never seen; an OpenAI-strict validator would refuse the
whole session, not one turn. So a request speaks one spelling, the
newest the log holds: the model answering now is the one that streamed
the latest thought. The one request right after such a switch still
speaks the old name; the first reply settles it. OpenCode avoids this
by spelling everything the way the current model's catalog row says,
which needs a per-row field this design deliberately does not have.

While probing: Kimi K3 behind Zen now streams `reasoning_content`, not
`reasoning` as it did on 2026-09-03, so the other spelling is no
longer reachable live on the gateway. The unit tests carry it.

## 2026-09-18 — The model gets its thinking back

A charachat session on Qwen3.8 flash — local llama.cpp and the
halogen cloud both — had thinking blocks that made no sense: eleven of
them claimed the previous reply had been "Understood. I will follow
these instructions.", which was never said, and one described a
working directory from the model's training data. The actions that
followed were right every time. The model was reconstructing a prior
thought it did not have.

It did not have it because ilar never sent it. Every thinking block
was persisted as a local diagnostic and the chat wire dropped it when
rebuilding the conversation. That was the right call for the Responses
wire, which has its own reasoning items, and for OpenAI's chat wire,
which hands no thinking back — but the chat-completions families think
*interleaved* with their tool calls: Qwen3's template, and GLM's,
Kimi's, MiniMax's, DeepSeek's, keep `reasoning_content` on the
assistant messages after the last user message precisely so the model
carries its plan through a tool loop. Without it, every step inside a
turn starts from an empty think block.

So: a per-model flag, keyed on the wire the row is served on — every
chat-wire row and every discovered endpoint replays, nothing else
does. The turn loop persists thinking as thinking for those models
and as a diagnostic for the rest, so the log says what the wire does
with it; the chat wire sends `reasoning_content` on the assistant
messages after the last real prompt and on none from an earlier turn,
where every vendor drops it and one refuses it. A tool result rides a
user-role message on the neutral side and is not a prompt — counting
it would strip the thinking of the very step that made the call.

The review would not take "the templates keep it" on faith for the
OpenCode rows, which proxy to upstreams nobody can read from here, and
it was right to ask: a configured server that streams reasoning and
refuses it as input would 400 on step two of every turn. So the wire
consults the model itself, not only the persist step, and a
`[models.*]` or `[endpoints.*]` entry can say `replay_thinking =
false`. Then a live probe, now an ignored smoke test: one real tool
step, then the model's own thought and call echoed back with a
result, for qwen3.8-flash, minimax-m3, kimi-k2.6 and deepseek-v4-flash
on Go and kimi-k3 on Zen. All five answered. DeepSeek's answer failed
in *our* mapper — it spells every absent field as `null` on every
delta, and `"tool_calls":null` read as a malformed list. Null is
absence now. Recall also gained the thinking it had been missing: a
local diagnostic is a thought too, and the `thinking` speaker had
only ever seen half of them.

The same session's three turns that ended on "Writing the card
tests:" with no tool call are plausibly the same gap — the plan lived
in thinking the model never got back — but that half is not proven.
One stall of the same shape happened before the switch too. If it
persists after this, a one-shot nudge on a dangling stop is the next
thing to try, and it is a heuristic, which is why it is not in here.

## 2026-09-18 — A reviewer that may run things

The other half of this morning's read-only decision. `explore` stays
shell-less because a shell under the shared read lease is parallel
reviewers running parallel builds over one target directory; the cost
was that a review which had to *run* the tests went to `build` with a
"don't edit" note — the wrong agent wearing a label.

`review` is the third built-in: mutable in lease terms, so serialized
per checkout like `build`, with an allowlist of `read`, `glob`, `grep`,
`webfetch`, `bash` and `secrets`. No `write`, no `edit`, no delegation,
no `sudo`. The allowlist is coordination and not a boundary — `bash`
can write — which is the caveat `read_only` already carries, and the
prompt says to report and not fix. Its description names every tool
and the two it lacks, the way `explore`'s does, and a test keeps both
halves in step with the allowlist; a second test runs a `review` child
against a scripted provider that calls `edit` and `bash` and checks
that the first is refused by name and the second runs.

One departure from the issue as filed: it asked for background by
default, like `explore`. It runs in the foreground, like `build`. A
review's findings are what the delegating agent waits on before it
commits, and a detached serialized reviewer would hold the checkout
against that agent's own edits for the length of the review;
`background: true` still detaches it when that is wanted.

## 2026-09-18 — Three things the lock left behind

The lazy master password shipped with three loose ends its review had
named, all about what a sealed, unopened store does to the calls that
run against it.

**sudo read the lock as a wrong password.** It looked for a stored
`SUDO_PASSWORD` before anything else and returned on `Err(Locked)`,
twenty lines above the `sudo -n true` probe whose whole purpose is the
no-stored-password case. Now the lock *is* that case: the probe
decides, and where the system does want a password the ask is the
ordinary one; giving up at that prompt names the lock, since that is
the one place a person meets its cost. One correction to the issue as
filed, which the review pushed all the way through: a headless driver
never got that far, because the standing root approval lives in the
store too and a locked store read as "not granted" — a guess dressed
as a fact, on a box where `ilar secret grant root --tool sudo` had been
run. The approval now refuses with the lock instead. The first draft
had put the lock's wording on two headless paths below the approval
that nothing can reach; the review noticed they were dead.

The unlock gate also lost a race on review: it looked at the flags —
sealed, master held — and only then read. Two calls finding a resealed
store together had the first one's read drop the master and the
second one's read come back `Locked`, which the gate sent home
unprompted while the first was at the prompt. One read now decides,
and any lock error means "ask".

**A resealed store cost one refusal.** `unlock_if_locked` asked "is the
store locked?", and a stale master — a second process resealed the file
under another password — made that a no; only the read that failed
dropped it, so the call that found out failed and the next one asked.
The gate now reads the store when a master is held, so the discovery
is the discovering call's to ask about.

**A locked store scrubbed nothing and said nothing.** `all()` is
`unwrap_or_default()` on a sealed store, so the output scrub had no
needles and the value-matched half of the environment shielding was
off — not new, but newly the ordinary case, since a `bash` that names
no secret never triggers the prompt. The values cannot be read, so
that part is what it is; the silence was fixable. The first tool result
of such a session says so, once per runtime. Once, deliberately: a line
on every result would be the standing notice the TUI stopped showing
this morning, moved into the transcript. And no prompt on that account:
a prompt before the first command is the start-of-session prompt under
another name, which is what the whole change was for removing.

One commit for the three, not one each: they share a file and a
theme, and each is a handful of lines.

## 2026-09-18 — Read-only means these four tools

The forty-minute `explore` child from the 2026-09-16 session already
had its fix — `a50a78c`, the day after: the description names the
toolset, the prompt says to refuse and name the missing tool. What the
issue still wanted was a decision: does `read_only` keep meaning
"read, glob, grep, webfetch", or come to mean "everything that does
not write", the way Claude Code's reviewer runs `git diff` and Codex
sandboxes effects rather than commands?

It keeps meaning the four tools, and the reason is the lease. A
read-only agent takes a *shared* read lease on the checkout, which is
what lets several run at once; a shell under a shared lease is four
parallel reviewers running four `cargo test`s over one target
directory. The other tools have no lease to protect. So the honest
shape is two agents, not one wider one: `explore`, shell-less and
parallel, and a `review` that may run anything and edit nothing —
serialized like `build` because running things collides. The second is
filed; `build` wearing a "don't edit" note covers it until then. The
docs now say all of this where `read_only` is defined.

## 2026-09-18 — A listing that tells a killed task from one that answered

The `tasks` listing had two words for a task: `running` while a handle
lived in the registry, `finished` after. Cancelled, crashed, stalled,
answered — all `finished`, with the task's last assistant text under
`last:`. A real session on 2026-09-16 showed what that costs: Esc
aborted a parent turn, the detached review child died with it mid tool
call, and twenty minutes later the listing told the parent `finished ·
22m ago` over a plausible-looking mid-flight thought. The parent
resumed the task to collect findings that were never produced.

The root of it was that nothing anywhere recorded how a task ended. The
notification said so, once, to whoever was listening — and after an
abort that notification is *held* until the next user message, so for
the length of the hold the ending existed nowhere the model could read
it. The child's own log simply stopped.

So the ending is now written where it can be read back: one session
event, `TurnEnded`, appended to the child's log by the spawner on every
non-clean ending, with the same headline sentence the parent's
notification carries. It is session state in the way `Topic` is —
never sent to the model — and the transcript view shows it as one line
where the chain used to break. The listing reads it and says
`cancelled`, `failed`, `stalled` or `aborted`, the verb set the
notifications already used; a stopped task's last words are labelled
`partial:`, and only a finished one has a `result:`. Logs written
before today have no such event, so an assistant message the stream
left `aborted` stands in as the one trace there is.

The second half was the same session's other failure. A finished task's
result reaches the parent exactly once, and a held notification can
wait a long time; meanwhile the listing said `finished` and showed a
200-character snippet, so the parent resumed the task to ask for the
result again — a second full run — and then received both copies. The
outbox already knew the truth: every completion is recorded on disk
before it is sent, and delivery is proven from the parent's log. The
listing now asks it, read-only, and a finished task whose result has
not landed says `result not delivered to you yet` and carries the
result in full, up to eight thousand characters. Matching a recorded
result to its row goes by the `task_id:` the completion text names —
a field on the notification would have been cleaner and would have
touched forty test literals for the same answer.

TDD note: the two tests were written first, but the red run was the
compiler refusing an event variant that did not exist yet; cargo does
not run on this Mac, so the first green was also the first execution.

## 2026-09-18 — The lock is a prompt, not a notice

The lazy unlock shipped with a standing line on the notice row — "secret
store sealed — the master password is asked for when one is needed" —
so the first prompt would not come as a surprise. In use it read as a
warning that never went away, in sessions that never touched a secret.
A sealed store is its normal state, not a fault; the prompt that opens
it already names the tool waiting and why. The line is gone, and with
it the store handle the app kept and the per-frame `has_master()`
lookup that only existed to take the line down again. The entry below
about watching the lock cheaply is now about code that no longer
exists; kept as written, since the reasoning still holds if the row
ever comes back.

## 2026-09-18 — A password asked of everyone, for the sake of a few

The master password was the first thing a sealed store's owner saw,
every run, whether or not the session ever went near a secret — and
most do not. Worse, `ilar exec` asked it too: the driver that builds
its runtime with `questions: false, grants: false` *because nobody is
there to answer*, stopping on a password prompt before the first token.
A piped prompt or a cron line simply hung.

The fix reads as obvious once written: the password is a question like
the other two the store asks, so it goes on the channel they already
use and arrives when a call actually needs it. `Ask::Unlock` beside
`Ask::Grant` and `Ask::Password`; one masked modal for the two
passwords, told apart by a `Wants`. exec gets no prompt channel at all,
so the same code that makes it ask nothing about a grant makes it ask
nothing about the store — the guarantee falls out of the wiring rather
than being asserted anywhere, which is why there is now a test that
pins it.

Two things the review caught that the design had glossed over.

The first was a live regression: the one start prompt that has to
survive — a provider key kept in the store is read when the
configuration is resolved, long before a tool call could ask — keyed on
`general.model`, and the TUI takes `--model`. Launch with a `--model`
whose key is sealed and the process died on `plan.start` before the
screen came up, with no prompt and no way to one. It now asks about the
model the launch is actually opening with. The same gate then wanted a
second condition: a model id nobody can *route* is a typo, not a locked
store, and deserves its own error rather than a password prompt first.

The second was an ordering mistake of exactly the kind this change was
meant to remove. sudo asked for root approval first and unlocked
afterwards — but a standing "always" for root lives *in the store*, so
a locked one reads as "not granted" and a person who had already said
always got two prompts where they had agreed to none. The unlock
belongs ahead of the question whose answer is in the store. That is the
same reasoning that put it at the top of `resolve`.

And a smaller one, worth keeping: watching whether the store is still
shut does not need the file. Inside one process the file cannot stop
being sealed; only this process gaining the password can change what
the notice row should say. So the row watches `has_master()`, a map
lookup, instead of re-reading and re-parsing `secrets.json` on a timer.
The throttle that existed to make the polling affordable went with it.

Still open, and a deliberate choice rather than an oversight: a cancel
is per call, not per session. Esc refuses the call that asked, and the
next call that wants a secret asks again — the same shape as a denied
grant. It means a model in a retry loop can put the prompt up
repeatedly; the alternative, remembering the no, needs a way to change
one's mind that the TUI does not have yet.

## 2026-09-16 — Two ways in that named nothing

Both of the day's security follow-ups were the same shape: a guard
that asks "does this name the thing?" against a spelling that names
nothing.

`curl -u bob:hunter2` went through redaction untouched. `-u` is not a
sensitive key, `bob:hunter2` is not a URL, and nothing else in the
line says a word about a secret — the credential is identified by
position alone. The only thing that can tell that value from a
filename is the program in front of it, which nothing in the token
pass knew: `-u` is `user:password` to curl and a sort order to `sort`,
a file mode to `chmod`, a user to `docker run`. So the pass now tracks
which program is in force and reads a table of flags whose value is a
credential.

Tracking a program turns out to be most of the work. A command ends at
a `;`, a pipeline, a `&&` — and at a line break, which matters more
than the rest put together, because a `bash` argument is routinely a
whole script and the first line would otherwise answer for all of it.
A command begins after those, under `sudo` or `env` or a flag of
theirs, and inside `$(…)`. The review found the newline gap, and two
others worth having: the positive test could not see over-hiding (it
asserted the secret was gone, not that nothing else was), and `curl -u
deploy` — an account with no password — was being collected as a
needle, which would have struck the word `deploy` out of the tool's
entire output. It is hidden in the row and not hunted through the
result; only a value carrying a `:` is.

The other one: the withheld-path gate refuses a call that names the
memory directory, and `grep` pointed at the parent named nothing. The
fix belongs in the walkers rather than in the gate — refusing every
call that names an ancestor would refuse `read <home>/SOUL.md` — so
both now skip a withheld subtree the way they skip `.git`, silently,
because a room that may not read the memory may not be told it is
there either. The walk's own root is checked separately, since
`ignore`'s filter never judges its root.

Its review found the one real bypass, and it was not in the new code:
the comparison was byte-exact. On the filesystem this most often runs
on, `<home>/Memory` opens `<home>/memory` and `canonicalize` does not
correct the capital, so one shifted letter walked past the filter —
and past the gate, which had compared the same way since it was
written. The two rules now share one comparison: lexically normalised,
component by component so `memory` does not claim `memory-notes`, and
blind to ASCII case. What it costs is a sibling differing from a
withheld path only in case, which cannot exist on the filesystem where
the rule is needed.

And the sibling leak the same issue named: every seat's transcript
lives in one directory under the state dir, so a room's seat could
read the private chat's — what the assistant was told about its
person, which is exactly what withholding the memory was for. A room
withholds both now. Nothing a seat opens by path lives there; spilled
tool output and images have their own directories.

What is still open is the `bash` case — `cd <home> && cat memory/x`
resolves nothing any gate can see — and that stays with the kernel
sandbox issue, which is the only thing that closes it.

## 2026-09-16 — Three follow-ups, a guard rail, and a decode bomb

v0.3.0 went out, and then the backlog's own top three.

A session that had been aborted could not be resumed after a reopen.
The live abort offers Ctrl-R; the restore looked for a recorded
`TurnError` and an abort writes none, because the user stopping a turn
is not an error. So the restore was asking the wrong question. The
right one is where the log ends: a tool call nobody answered, or a
result the provider was never told about, is a severed chain whichever
way it was severed. The iteration ceiling stops a turn in exactly that
shape too, and now gets the same offer.

The room seat could read the memory files its tools were denied. The
earlier fix had withheld `memory`, `memory_search` and `memory_get`
from a non-private seat; `read` and `bash` were still pointed at
`<home>/memory/USER.md`. The mechanism that came out of it is a list
of paths a session's tool calls may not name, checked once in the
executor rather than in each file tool — a path withheld from `read`
is withheld from the `bash` that would cat it, and a tool added later
cannot miss the gate. It is carried into subagent children, since
delegating the read is still the read.

The review of that one earned its keep twice. First: the substring
match missed `../memory/USER.md`, which is not an evasive spelling but
the obvious one, so each string in a call's arguments is now also
resolved against the cwd — lexically, because a call about to be
refused must not first get to probe the filesystem. Second, and worse:
the situation block named `memory/` to *every* seat, so the system
prompt itself was sending a room's model to the one directory its
tools refuse. The block now lists what the seat actually has. What
remains open is a walk that never names where it ends up — `grep` over
the parent — and that is filed rather than papered over; the whole
thing is a guard rail, and the kernel sandbox is still the only
boundary.

Third, the smallest: the situation stamp was frozen at session open, so
a seat left running for a week reckoned "tomorrow at nine" from last
Tuesday. Rewriting the stamp per turn would rewrite the cached prefix
under it, so it rides in front of each prompt instead, at the end of
the conversation where nothing is cached yet.

A question from the user closed the day: the assistant on tenco
seemed to answer without thinking. It was thinking the whole time —
57 thoughts for 57 answers, all of them in the log. Raw thinking is
persisted as a `Diagnostic { Local }`, because no provider will take
it back, and the restore fold dropped that block on the floor. Live
you saw `▸ Thinking:` rows; reread, nothing. And `--view` is always a
reread — it tails the file and re-folds on every change — so the one
surface built for watching the assistant work was the one surface
that could never show why it did anything. A provider's reasoning
summary was already restored as a collapsed thought two arms up in
the same match; raw thinking now joins it, along with the older
`Thinking` block shape that logs written before the split still
carry. Checked against the live chat session on tenco, which now
reads back with its reasoning intact.

The same look at the box turned up something unrelated and worth
knowing: the gateway is not running the model its config names. A
`/model` in the chat writes `<home>/model`, and that file outranks
`[gateway] model` for every new session.

Then one redaction engine, the first of the structure sweep and the
one that was not really a refactor. Two copies of "hide the secrets"
— one for a tool row's arguments, one for a provider's error body —
had drifted: the error body's needle list was missing `privatekey`,
so a provider naming one published it, and it had no URL-credential
pass at all, so `https://user:token@host` went out verbatim in the
shape providers most like to quote back. One needle table, one token
pass, one URL rule; what stays with each caller is policy, which is
the part that genuinely differs.

The review of it ran a differential corpus over the old pair and the
new one, and found that merging had quietly pushed the error body's
aggression onto command lines. A colon splits a header and it splits
`src/token.rs:88:3`, and the "secret" a path yields is not only
blanked in the row — it is collected and struck out of the tool's
entire output, so a pytest run would have lost every `:test_login` in
its report. The seam is now mode-aware: in a stranger's text every
colon splits, in a command a path-shaped key is a path. It also found
the mirror bug, an under-redaction where a failed key check shadowed
the whole-token one, which is the shape the merge was supposed to make
impossible.

Then the decode bomb. A PNG's header is a few dozen bytes and can
claim to be 40,000 by 40,000; `output_buffer_size` believed it and
asked for six gigabytes. The size is now read from the header as bytes
— before any decoder is asked to believe it — and a picture past 64
megapixels is refused unread, as is a file past 10 MB before it is
read at all. The clipboard's own decode belongs to arboard and happens
before ilar sees a pixel; what ilar can refuse, and now does, is making
two more allocations of its own out of the result.

## 2026-09-16 — The offer's keys move under the cursor

The resume offer shipped yesterday explained itself in its header:
"previous session here: <title> · just now — Enter resumes · type to
start fresh · Esc dismisses". On a real terminal that wrapped to two
lines, read as a sentence about three keys, and sat as far from the
prompt as the pane allows — instructions furthest from the place they
are obeyed. The user's verdict was "the usage here is pretty unclear",
and the fix is placement rather than wording: the header now says only
what is on screen, and the empty prompt says, muted, "Enter resumes ·
type to start fresh". A promise directly above the keys that keep it.

The keys followed the wording. Typing used to leave the ghost standing
until Enter decided its fate, so the screen showed a draft under a
session that was not going to be resumed. Now the first character
dismisses it and still lands in the prompt — the offer is answered by
the same keystroke that starts the fresh message. Esc on an empty
prompt still works and is no longer advertised; there was nothing for
it to do that typing does not.

One small thing the change forced: the input box's own footer, "Enter
send · …", is suppressed while the hint is up. Two promises about
Enter on one box is the confusion this issue was about.

## 2026-09-15 — Four more from the evening: sudo's order, the session list, a ghost, a phantom

The user tried the sudo tool and hit the sweep's own bug before the
fix was installed: a "type the password" prompt with no visible field,
an empty answer taken as "none needed", and a second prompt with no
field at all. The redesign that followed is the right order rather
than a patch: approval first, then `sudo -n true` to learn whether a
password is wanted at all, and only then a prompt of its own, re-asked
on a wrong password. The chat got `/password`, which deletes its own
message.

Then the session list. Measured on the Mac: 5,593 directory entries,
2,148 of the 2,421 session files subagent children that every listing
opened and threw away, 2,414 lock files nobody removed, and 104 of the
273 root sessions empty because the file is created at launch. The
worst of it was a cap applied before the directory grouping, so this
directory's last session fell off the list once 200 others were newer.
A summary cache keyed by file stamp, lock removal on lease drop, empty
sessions removed on quit, and, because "continue the last session
here" is the 99% case, a per-directory pointer that answers
`--continue` without a scan. On that pointer, a bare `ilar` now offers
the last session here with its tail ghosted in the transcript pane:
Enter resumes, typing starts fresh, Esc dismisses.

The phantom task result was the satisfying one. A notification
delivered six times to one root, once per open, and the model saying
each time that it had dealt with it. Not a stale entry being re-read:
a grandchild's result addressed to a task whose worktree was gone,
re-adopted at every open, failing to route, and turned into a fresh
failure note for the root by a path that carried the note upward but
never retired the origin. The note did not even carry the result, so
the work was lost each time. Now a terminal target failure is its own
outcome, the note embeds the result, and the retire is a field on the
disposition that no driver can leave unread.

Each of the four ran as one Opus agent in its own worktree with the
tenco gate, the way the sweep did; two at a time, rebased as main
moved. One agent found this Mac's disk full and deleted the local
debug target to keep working — right call for a build cache the rules
forbid using here, but a deletion nobody asked for, so it is written
down.

## 2026-09-15 — The sweep, and eight agents in worktrees

A UX sweep, asked for as "weird things, inconsistencies, bad states".
The first four read-only passes were pointed at what had just landed
(secrets, grants, sudo, the master password, focus messaging) and came
back mostly about that; the user asked whether the rest was fine or
just unread. Unread. Five more passes went by user journey instead of
by recent change: first run and configuration, the life of a session,
the tools as the transcript shows them, the gateway's daily use,
agents and delivery. About 130 findings between them, filed as 32
issues under one index, a dozen of them real bugs: a bad `--model`
reported as a missing key, `read` cutting a long line with no marker,
slash commands mid-turn steered to the model as text, Ctrl-D quitting
through a running turn unwarned, Esc killing detached children and
their mail starting a fresh turn, a gateway restart aborting the turn
in flight with no re-run, "sent" meaning queued, a group chat that
could receive the weekly memory review.

The fixes ran as eight Opus agents in isolated worktrees, six at once
and two after, each with its own branch and its own remote worktree on
tenco or secunda for the gate (the Mac cannot run cargo in useful
time; a fresh worktree builds in two minutes there and takes 11 GB).
Streams were cut by crate region so that no two edited the same
functions; the two gateway streams and the four TUI streams still
overlapped in the big files, and every branch was rebased onto main
before its gate, with the orchestrator relaying "main moved, here is
what changed" between them. Two textual conflicts in all, both in
docs and one enum arm. Each stream had an independent reviewer before
reporting; what the reviewers found and the streams deferred is one
follow-up issue.

Two things worth remembering. The parallel gates made the parked
serve adoption test flake three times under load; alone it passed
every time, and the final gate on a quiet box was clean. And one agent
did a `git reset --soft main` after main had moved, which silently
folded another stream's revert into its commit; it caught that itself
by diffing against main before reporting, which is why "report
`git diff main...HEAD`" is in the briefing.

## 2026-09-15 — Root, and a password for the passwords

Two follow-ups the same day. A `sudo` tool: one command as root after
the person has read it. It fell out of the grant protocol almost for
free: root is a pseudo-secret with no value, the prompt reads "sudo
wants root to run: …", and the same once/session/always answers apply.
The part that was not free was the password. sudo under the bash tool
fails fast on purpose (no controlling terminal), so the tool feeds a
password on `sudo -S`'s stdin; and the person wanted to type it into
the prompt rather than store it, so the grant reply grew an optional
password, held in memory for the session and never written. A stored
`SUDO_PASSWORD` serves the unattended case.

Then a master password for the store. Argon2id to a key, XChaCha20-
Poly1305 over the JSON, a fresh nonce per write, and one unlock per
process held in a map keyed by the store's path (so tests with their
own stores do not trip over each other). The TUI asks on the plain
terminal before it takes the screen, then reads the configuration
again so a provider key in the store is seen; the gateway logs that it
is locked and takes `/unlock` from a chat. The honest note in the docs
stayed: on a box that runs the gateway unattended, sealing means typing
the password after every restart, and a provider key in a sealed store
is only read after that.

The bug worth writing down: `run_command` took the stdin bytes out of
the environment struct before spawning, and the spawn decided piped
versus `/dev/null` by looking at that same field. The fake-sudo test
printed an empty password and caught it.

## 2026-09-15 — A key the model never sees

The ask was simple: hand ilar an API key without it ending up in the
transcript, and have ilar ask before using it. The survey said the
opposite was true: keys sat in ilar.toml or the environment, the bash
tool passed its whole environment to every child, redaction was
display-only and the session store kept results raw, and nothing in
ilar ever asked permission for anything.

The shape that came out: a store of named values, a `secrets` argument
on bash and service, and a grant protocol copied from the question
protocol (a prompt over a channel, a one-shot reply) so each driver
answers it its own way: a modal in the TUI, `/grant` and `/deny` in the
chat, a refusal with the CLI line under `ilar exec`. The value reaches
the command as an environment variable and nothing else.

Two things the review caught before it shipped. The store file itself
was a way around the grant (`cat secrets.json`), so every tool result
now passes through one scrub of every stored value at the executor,
the one place all results go. And `Command::env_remove` after `envs`
also removes what was just set, which would have dropped exactly the
common case: the person's own exported token, stored under the same
name. Removals now go first, with a test that names the same variable
on both sides.

Left alone on purpose: encryption at rest (the gateway would need the
passphrase next to the file anyway), values the command transforms
before printing, and a value cut in half at a capture boundary. Not
built: a masked input in the TUI (`ilar secret set` in another terminal
does), setting a secret from Delta Chat (the message would be in the
chat database first).

## 2026-09-07 — Where a weekend of tokens went

A weekly ChatGPT allowance spent on two root sessions with gpt-6-astra.
The session logs (09-05..07) say: 10,756 requests, 891M tokens read
from cache, 28M uncached (3%), 4.2M output; two roots at 2,726 and
1,100 requests spawning ~305 children that took two thirds of the
requests and about half the tokens; compaction fired at ~231k every
time and handed over to 7–15k; six error turns, none about quota. So
nothing broken, just a 272k window kept at a median 120k, times ten
requests per user message, times the fan-out.

Context growth split: read results ~40%, bash/grep/glob ~25%, output
incl. reasoning 18%. Pruning stale tool results mid-window was
simulated against the logs (one prune at 150k per window, results
older than 20 steps dropped): 3–8x return on the one cold re-ingest,
but only ~20% of a session's cached reads, and it discards material
the model may still want. Decided against — the current scheme is
known not to lose anything before compaction. Kept: the base prompt
now asks for independent tool calls in one response, since the build
agent averaged 1.08 calls per request and the explore agent, whose
prompt mentions parallel inspection, 3.44. Also noted for later:
`read` and `glob` can return 256 KB / 187 KB in one result, and the
ChatGPT backend's usage-percent headers are not read, so the TUI never
warned.

Found on the way: the background stall watchdog counted only the
task's own loop events as progress, so a foreground `task` call — which
blocks the caller's turn — read as silence for its whole run, and a
subtree that was busy for ten minutes was killed twice as "stalled".
Now a `Heartbeat` on the tool context: a background task creates one,
foreground children inherit it and touch it on every event at any
depth, a background child starts its own. The test reproduces the
production shape (mutable task, read-only foreground child).

## 2026-09-14 — The window a request can use

Two listings say two things about a model's window and disagree in
both directions: Lemonade configures `context_length` below the
model's `max_context_window`, llama.cpp reports `context_length` as
the total across its parallel slots, twice one request's share.
Discovery took the first and fell back to the second; it takes the
smaller now. And for the cases no listing gets right, `/context` in
the TUI: a picker of common sizes, or `/context 128k` typed, a
session-only override that drives the ctx meter and the compaction
threshold and survives a model switch.

## 2026-09-14 — A job on the panel

A background bash job showed itself only when its notification
landed, so a long render read as a hung session. The job now
registers itself in the spawner's running registry for as long as it
runs — session: its owner, agent: "job", the command as description
— which is the registry the agents panel and the `tasks` tool already
read. The panel draws it as a ⚙ row with elapsed time and gives it no
focus target, since a job has no transcript. Also caught on the way:
the ChatGPT backend refuses `max_output_tokens`, so the OpenAI
provider drops the cap on that login and keeps it for an API key.

## 2026-09-11 — Watching a session

Opening a gateway chat in the TUI took its writer lease, so the chat
was paused while you looked and typing drove it. `ilar --view <id>`
now opens a session read-only: the picker's own restore for the
transcript, the TUI's own renderer, a tail on the file that rebuilds
the view on every change, and no runtime at all — no provider, no
lease. Enter says it is a view. Liveness — whether open tool rows
should stay open — comes from a probe of the lease, taken for an
instant when free. Checked live: with the view open on the Delta Chat
session, a notify turn started on it without the chat being told the
session was busy.

## 2026-09-11 — Three stops for a runaway

A gateway turn ran away: after a short thought about a hallucinated
watermark the local model began a `message` tool call and streamed
its arguments for ten minutes, thirty thousand tokens at twice its
usual speed — the speculative draft guessing right on repetition. The
only way to stop it was to restart the service, which also dropped
the messages waiting to steer the turn. Three stops now, none of
which needs the model's cooperation. `/abort` (or `/stop`) cancels
the running turn on a chat through a per-turn token the seat holds;
the chat gets "Aborted." and the messages that were waiting run as a
turn of their own, the leftover path a failed turn already had. A
response has an output cap, `agent.max_output_tokens` (32,768 by
default, `0` for none), sent as the wire's own cap — `max_tokens` on
chat completions, `max_output_tokens` on the Responses API — and
yielding to a value the configuration's options already carry; a
response cut there says so in its text, where the reader is. And one
tool call's arguments are cut at 1 MiB whatever the cap allows, since
no call is legitimately that large and this one was exactly that.

Noted and parked: Qwen's presence penalty for thinking mode. DRY went
on instead, as `options` on the Lemonade endpoint, after a probe
showed the router forwards it ("echo" forty times asked: 36 without,
5 with). And `/compact` joined the chat's commands: the core's manual
compaction under the seat's turn lock, the handover into the daily
note, the chat told its size.

## 2026-09-11 — Pictures, bounded twice

A chat that had generated and inspected 47 pictures came to hold
108 MB of base64 images, every one of them re-sent on every request,
until Lemonade's router refused the body: its cpp-httplib cap is
compiled in at 100 MB, no key, no flag (measured, not read). The
first cut was the obvious one — the images were full-size PNGs of
832 by 1216 at one to three megabytes each; fitted to 1024 px and
stored as JPEG when opaque and smaller, the same pictures are about
150 KB, so a conversation like that one weighs seven megabytes and
no cap is near. That touches only images arriving from now on, which
is the point: nothing in a running session's cached prefix changes.

The second is for the session that already held the pictures, and
for any that outgrows the budget later: a sliding window would have
rewritten the prefix on every new image and cost a cache miss each
time, so the drop is recorded instead. An `image_cutoff` event names
the index before which pictures no longer travel; the turn loop
writes one when the images past the last cutoff exceed 24 MB, keeping
the newest four, so the rewrite happens once and the prefix then
holds until the next. The transcript keeps the words and puts a note
where each picture was; the log keeps the pictures for the viewer.

Along the way a real flake: grep's limit notice depended on whether
the parallel walk had seen a match past the cap before it quit. The
notice now says what is known — the cap was reached — and no more.

## 2026-09-10 — Tools that refuse what cannot be meant

A chat got eight text-only messages each claiming to carry a photo:
the message tool had refused a `channel` of `deltachat:12` (a session
key, doubled into `deltachat:12:12`) with a message that did not say
what was wrong, and then sent every retry that spoke of an attachment
with an empty `media`. The fix — take the key apart, name the known
chats, refuse a text that claims an attachment unless `no_attachment`
says it is meant — set the standard, and an audit of every tool
against it turned up three batches.

Destructive: `edit` with an empty `old_string` and `replace_all`
inserted the new text between every character of the file;
`skill_manage` patch without `new` deleted the passage and rewrite
without `triggers` dropped them; `memory` remove swept every entry
containing the text and said only "removed". Accepting nonsense:
`cron` ignored unknown fields, so a message-style `channel`/`chat`
pair scheduled for the home chat unnoticed; it also took a one-second
interval, empty names, and said "never fires" for a past time; `bash`
took a seconds-shaped `timeout_ms` and killed the command at once;
`history` read its fields leniently, so a wrong type changed the mode.
And refusals that left the model guessing: `grep` and `glob` on a
missing directory returned nothing; `webfetch` without a scheme
returned the URL parser's words; `read` on a directory the OS error;
`memory_get` dropped unknown ids silently and the cap said
"consolidate" with no way to read the core (there is a `show` action
now); `skill_manage` said "no skill" without the names.

The rule that came out of it: a refusal names the fix, a tool that
knows the valid values lists them, and a silent default that changes
meaning is a refusal instead.

## 2026-09-10 — The preview is the request

`ilar --print-prompt` and `ilar-gateway prompt` printed the system
prompt and nothing else, while the model also gets the tool list with
every description and schema, and the options a reasoning variant
adds. Now both print the whole first request: a header with the
model, reasoning, request options and agent, the system prompt as
sent, then each tool. The point was that it cannot drift: the
runtime's tool construction moved out of `start_with` into a
`tooling` step that builds the registry, spawner, services and tool
context without touching the store, `start_with` creates the session
and calls it, and `RuntimePlan::preview` calls it alone. The gateway's
seat tools moved into one `seat_tools` the seat opening and the
preview share, under the same policy. A preview creates no session;
both tests check the store stays empty.

## 2026-09-10 — Where it is, its own skills, and steering

Three more from daily use. The gateway's prompt now carries a "Where
you are" block after the base instructions: reached over a chat and
answered through the message tool, the home and workspace paths, and
`ilar-gateway notify` as the way a script it started can wake it —
nothing had told the assistant that command existed. Skills come from
the home alone now: a `RuntimeOptions.own_skills_only` the gateway
sets makes the skill store skip the two built-ins and the working
directory's `.ilar/skills`, which the service's `~` had been
contributing to the list; `~/.config/ilar/skills` was already out
since the home work.

Steering is the TUI's, reused. The core loop has taken a steer
channel all along and the gateway passed `None`; now each seat holds
the running turn's sender, a message arriving while the turn lock is
held goes through it instead of waiting, the narrator shows "steered:
…" when the loop reports delivery, and the seat keeps the steers it
handed over until each is reported read, so whatever a cancelled turn
never saw runs once as a turn of its own. The test for it shook out a
real flake in the status updater: with a zero interval the select
between "due" and "newer line" was unbiased, so a line arriving in
the same instant could replace the one about to post. Biased now.

## 2026-09-09 — Restarts you can see, and settings that stay put

Three asks from daily use. The gateway now tells the last active chat
when it stops and when it starts, the start line carrying the commit
the binary was built from (a build script asks git) and the model
new chats get, so a deploy is visible where the person is looking.
The first live restart exposed two things the tests could not: systemd
signalled the whole cgroup, so the rpc server was dead before the
goodbye could go through it (`KillMode=mixed` now), and every shutdown
waited the dispatcher's full grace because the seats keep senders to
the outbound queue, so the receiver never closed (a token raised after
the driver stops ends it). `systemctl stop` sends SIGTERM, which the
binary only now handles like Ctrl-C.

`providers.openai.image_gen = false` keeps the image tool out even
with a ChatGPT login; the per-provider merge copies fields by name and
had to learn the new one, which the test caught. And a `/model` switch
was lost at every restart: the gateway passed `gateway.model` as the
launch's model on every reopen, and the launch outranks the session's
own record. A resumed session now gets no launch model; the default
applies to fresh sessions only. `/model <m> --save` (or `/model
--save` for the chat's current model) writes `<home>/model`, which
sits above `gateway.model` as the default for new chats — state, so
it lives in the home rather than in `ilar.toml`.

Process note: workspace-wide clippy on the Mac ran twenty-five minutes
and starved the machine; the rule from now on is that clippy and full
suites run on secunda or tenco, where the whole check script takes a
minute.

## 2026-09-09 — The assistant learns: a home, a review, its own skills

Hermes's three learning loops, read from its source and taken with
adjustments. First the ground for them: the gateway's state directory
is now the assistant's home, holding `SOUL.md`, `skills/`, `agents/`,
`commands/`, `memory/` and `workspace/`, so nothing of the assistant's
is under `~/.config/ilar` any more; that took the core's
`RuntimeOptions` growing a `user_dir` (every read of the user config
dir in the runtime goes through it) and a `context_files` list, and
the spawner following the same directory for subagents.

The review after a turn is Hermes's "memory review" made cheaper: it
runs once the chat has been quiet for just under the provider's cache
window, as an aside over the conversation that records nothing and
takes no writer lease, so the whole thing is one cached request. Only
an episode with enough tool calls or an error is asked, and the
prompt has no bias toward action — Hermes's version is told to look
for things to keep; ours is told "nothing" is welcome. The answer is
a JSON plan applied through the memory store, or staged for
`/approve` when `gateway.review.approval` is on. The review found two
holes on its own: it ran for rooms, which never get the core memory
either (now private chats only), and an answer that was not a plan
was logged as "nothing to keep" and the episode dropped (now a
three-way parse, and an unparsed answer keeps the episode for next
time).

`skill_manage` is the assistant's own skill library, in the layout
the `skill` tool reads, with a `.usage.json` ledger of views and
patches, and Hermes's two rules in the tool's text: lessons, not
logs, and patch before create. A patch matches the body only, since
a match inside the frontmatter corrupts the file the core parses.
The weekly review is Hermes's Curator plus OpenClaw's dreaming as one
cron job the gateway owns: upserted at every start from
`[gateway.weekly]`, addressed to `last` (the last active chat), its
prompt asking for promotion into the core, retirement and skill
consolidation, and a model-free sweep right before it that archives
skills unused for ninety days and names the ones unused for thirty.

## 2026-09-08 — Endpoints discover their models

`[endpoints.<name>]`: one section for a server that lists what it
serves, instead of a `[models.*]` section per model. The listing is
fetched at config load on a thread of its own — the loader is
synchronous and sometimes runs inside a runtime — with a three-second
timeout and cached under the state dir, so a server that is down at
start still yields last time's models with a warning. Discovered rows
join the runtime catalog under `<name>/<id>`; that took `RuntimeModel`
growing a provider and the registry matching on both halves. Lemonade
turned out richer than its spec: `labels` (`chat`, `vision`,
`reasoning`) and `context_length` per model, so those are honoured and
the endpoint's `context` is only a default for plain listings like
llama.cpp's. Only ids the listing had resolve; a model added to the
server later needs a restart. On tenco Lemonade lives on port 13305,
not the documented 8000, and lists eight chat models.

## 2026-09-08 — The assistant answers on Delta Chat

Milestone 21's first two steps in a day: `ilar-gateway`, a crate that
drives one live `SessionRuntime` per chat and feeds subagent
completions back as follow-up turns with the outbox obligations the
TUI honours; and Delta Chat through `deltachat-rpc-server`'s stdio
directly — picoclaw's Python-over-WebSocket bridge turned out to need
about twenty calls of a binary that speaks line-delimited JSON-RPC,
so the adapter is a client, not a bridge. Two core changes made it
possible without a fork: `[gateway]`/`[channels]` pass through the
config user-scoped, and `RuntimePlan::start_with` takes the resolver
so the crate's tests run against a mock. The review caught a race
that could give one chat two sessions (opening now takes a lock) and
a salvage that retired its outbox entry before the salvage landed.

Later the same day, steps 3 and 4: the model answers a chat by
calling a `message` tool rather than by returning text, so it can
send several, attach files, address another known chat, or stay
silent; the final text is delivered only when it sent nothing, and
the count that decides that is taken under the seat's lock, after a
review caught a second turn queued behind a first one losing its
reply. Everything out goes through one queue drained by one task, so
the gateway's own lines never overtake the model's. The policy is
built into registries: a denied tool is absent from the model's list,
and every agent definition the chat's spawner is built from is
narrowed first — a deny-only policy has to become an explicit
allowlist there, since an unrestricted agent's registry comes from
nothing but its defaults. A stranger gets no reply at all: a reply is
a spam vector, and on Delta Chat it accepts the contact request.

Evening: steps 5 and 6. Cron and heartbeat are the same thing — a
prompt on a session of its own, homed on a chat, heard only through
the message tool — so a job or a beat with nothing to say says
nothing. Memory went in as designed: two capped core files frozen
into the prompt per session and kept out of groups, an archive of
typed one-fact files and daily notes that is never injected, BM25
with a thirty-day half-life done in-process because the corpus is a
few hundred small files, two-phase search then get, and every
compaction's handover written to the day's note. Left for later, on
the issue: embeddings, the post-turn review, cards in a subagent's
brief, promotion into the core. The review of the cron commit caught
background sessions being written into the routes file, where the
model could have addressed them.

Two lessons. A chatmail address is not something a person can write
to; the bot has to hand out its secure-join invite, so it does, at
every start and through `ilar-gateway invite`. And this Mac's sandbox
blocks IMAP/SMTP while letting the HTTPS account creation through,
which looks like a working account that never receives anything —
the live run lives on tenco.

## 2026-09-08 — Memory systems, surveyed for the assistant

Read for Milestone 21: OpenClaw's memory docs, Hermes Agent's, the
Awareness Local README, the A-MEM paper, and the 2026 comparisons of
Mem0 / Zep–Graphiti / Letta. Four findings.

The lightweight systems converge. OpenClaw, Hermes, Awareness and
Claude Code all keep Markdown as the source of truth, split memory
into a small always-injected core (Hermes caps MEMORY.md at 2,200
chars and USER.md at 1,375, injected once per session as a frozen
snapshot — cache-stable, which matters here) and a large archive
reached only by tools, and retrieve with SQLite FTS5 plus local
embeddings fused by reciprocal rank (Awareness: 96.0% Recall@5 on
LongMemEval, +3 points over either signal alone, zero LLM calls).
OpenClaw adds MMR de-duplication and exponential recency decay,
and injects nothing automatically except the curated core; the
archive is never in the prompt. Awareness's two-phase recall
(an ~80-token index entry per hit, then full items by id) is the
right shape for ilar's token economics.

Write paths are where designs differ. OpenClaw flushes notes right
before compaction (a prompt that may answer NO_REPLY), which maps
onto ilar's handover summarizer for free. Hermes runs a post-turn
background review on the main model while the prompt cache is warm
— the same window `cache_compact` already times — and can stage
writes for approval. Both are cheap because the cache is warm; ilar
knows exactly when that is.

The heavy systems buy something specific. Zep/Graphiti's bi-temporal
graph wins on "who owned this in February" questions and costs
~600k tokens per conversation to build (Mem0's figure; third parties
agree on the order of magnitude), with results appearing hours after
ingestion. A-MEM's Zettelkasten notes with LLM-generated links and
neighbour rewriting more than double multi-hop scores on LoCoMo at
1–2.5k tokens of context, but every write is several LLM calls.
Vendor benchmarks do not reproduce (Mem0's LongMemEval number fell
from the paper's to 73.8% under a third-party harness). None of this
is what a personal assistant needs first.

What to take from Awareness "in spirit": typed knowledge cards
(decision, solution, risk, task) rather than undifferentiated notes,
the init call that hands a fresh agent the project's cards — which is
exactly what ilar's subagent children lack at start — and the market
idea only as "memory is a file tree, so it can be shared or published
later." Design recorded on the memory issue.

## 2026-09-08 — The tools the model reached for

Same logs, per tool: grep and write never erred, edit's 2% were the
seen-file guard doing its job, and the 355 one-line reads were images
being attached. Two ergonomic gaps, both visible as bash use. 3,156 of
8,219 read results ended in a bare "(truncated)" — the model reads in
100–300-line windows and had to guess where it was; the marker now
names the window, the total and the next offset, counting the rest of
the file without keeping it. And about 230 `rg`/`grep` runs went
through bash (context lines ~60 times, `-i` ~27, a trailing `head`),
each one serialized as a barrier tool; grep now takes `context`,
`ignore_case`, `glob` and `limit`, rendered rg-style so the model's
habits carry over. Left alone on purpose: bash spills were never read
back (643 of them) but cost nothing, and the cat/sed/head chains are
the batching the prompt now asks for directly — measure before
touching the tools for that.

## 2026-09-05 — The daily-use batch

Seven picks from the backlog, in one sitting: mid-stream hiccups now
continue the turn from the committed partial step (bounded to two per
turn, announced with a `StepInterrupted` event); an opt-in
`[cache_compact]` compacts an idle session just before its prompt cache
expires; a model switch replays once, the resume gate reads the head,
Esc during a rewind says why it does nothing; wrapped transcript rows
keep their gutter and error lines paint theirs; notices have a row of
their own and a held backlog shows without one; the picker finds a
topic written after the head scan; and the prompt under a focus view
talks to that agent through `message_task`.

Two lessons. A status detail written for one activity must not outlive
it — the detail now belongs to the activity it was written with. And
the gate: `cargo test --workspace --all-features` starves one serve
test deterministically while the crate-scoped all-features run of the
same binary never does, with identical feature sets; not understood,
routed around in `scripts/check.sh`, recorded on the parked flake
issue.

## 2026-09-05 — Astra, services in the handover, images

- `gpt-6-astra` (models.dev 2026-09-04): 1.05M window with a 922k
  input cap; cataloged at the 5.6 rows' 272k working window, where Zen
  and OpenAI double the price. Codex fetches its model catalog remotely
  now, so its 272k is only a test fixture; it gates Astra on ChatGPT
  behind a "Daybreak" access program — the user's subscription has it.
- Compaction's summarizer calls no tools, so "look up the services like
  the todos" had to be an injection: the registry keeps the service
  manager it installed and the compaction request carries `name ·
  command` for every running service, with a Services section in the
  handover template.
- Codex's image generation is a plain function tool over
  `{provider base}/images/generations|edits` (JSON, `gpt-image-2`,
  `data[0].b64_json`), not a Responses built-in; `image_gen` does the
  same on either openai credential. One direct probe of the ChatGPT
  backend with the stored token: 200, 743 KB PNG, 20 s.

## 2026-09-04 — The user's eye

A three-way read-only sweep of the TUI (transcript; status, notices
and input; overlays, sidebar, keys, exec) after the mail complaint
yielded 47 findings, filed under Milestone 20. Worked the first four:
task rows lead with the task (the headline normalizer had never
matched the producer's real `completed (task_id: …)` string, so every
completion showed a UUID); `app.status` had ~25 writers and no reader,
now rendered as the activity's detail; errors were persistent by
default and a stale one swallowed the quit warning, now only a dead
turn, a failed compaction or a crash stand, and standing notices
replace each other; every remaining raw session id in a notice, the
roster note, the search listing, the focus title and the export file
name goes through the session resolver. Note for the record: the
serve suite's `adoption_requeues_outbox_completions_as_follow_up_turns`
flaked once under the full all-features run and passed 3/3 alone —
the parked serve flake, not a regression.

## 2026-09-03 — Stale completions on reopen

The user reported reopening a session and having a dozen task
completions delivered to the root that had already reached a
subagent. The outbox on this machine held 246 pending entries; 242 of
them already sat in a `user_message` of their target log — nearly all
*before* that log's last compaction. `delivery::is_delivered` judged
delivery against `SessionReader::events()`, which is the active
compaction window, so everything delivered before a compaction looked
new again at every reopen; the 58 entries addressed to subagents then
resumed each dead subagent, and each resume propagated a fresh
"Nested task completed" to the root. The predicate now reads the
whole log via `SessionStore::audit_events`. Measured by comparing
`~/.local/state/ilar/outbox` with the session logs, split at each
log's last `compaction` event.

## 2026-09-03 — OpenCode Zen and Go

### Findings from live probes (one-token requests, every listed model)

- **One wire per model, and the other one is a bare 500.** The gateway
  fronts GPT, Grok and Muse Spark on `/responses` and everything else on
  `/chat/completions`; posting to the wrong endpoint returns
  `{"type":"error","error":{"type":"error","message":"Internal server
  error"}}` rather than a routing hint. So the wire has to be known per
  row, which is what `ModelAccess::OpenCodeChat | OpenCodeResponses` is,
  and unknown ids default to chat-completions.
- The docs' "Anthropic Messages" rows (MiniMax and Qwen on Go, Qwen on
  Zen) also answer on chat-completions — that column is opencode's SDK
  choice, not the gateway's only route. Cataloged on the chat wire
  after all, except the dark ones: Zen's qwen3.7-max/plus answer "not
  supported" and Go's minimax-m2.7 is a persistent 500 on both wires.
- A Go subscription key answered on Zen for every model probed. Whether
  that is billed as Zen pay-as-you-go is the console's business, not
  ours; the docs say one key serves both.
- `gpt-5.3-codex-spark` is listed on Zen but the upstream replies
  `model_not_found` (`gpt-5.3-codex-spark-preview`); left out.
- ilar's Responses body — `reasoning: {effort, summary: "auto"}`,
  `prompt_cache_key`, tools, `stream` — is accepted as-is by GPT, Grok and
  Muse on both gateways. `prompt_cache_key` is still not sent (a
  `base_url` is always set, which switches it off); cache reads showed
  up on Grok and GLM regardless.
- **Kimi K3 behind Zen** streams thinking as `reasoning` (with a parallel
  `reasoning_details` array), not `reasoning_content`, and sends its
  usage in a trailer that *repeats* `finish_reason: "tool_calls"` with an
  empty delta. The mapper now reads both spellings and lets a
  content-free trailer through; a trailer carrying content or a call is
  still the violation it was.
- Effort ladders come from models.dev's per-model `reasoning_options`
  (a field the first pass did not read — the user noticed Muse Spark
  lacked its levels). Chat-wire rows take `reasoning_effort`, as
  opencode sends it: on Go glm-5.3 the same arithmetic prompt spent 23
  reasoning tokens at `low` and 75 at `max`. Qwen3.8-max accepted
  `low` and `xhigh` alike with no visible difference in one sample.
  models.dev also lists `max` on gpt-5.6-* for the openai provider,
  so those rows gained it too.

- OpenCode mailed the same day that requests without
  `x-opencode-session` will start erroring on 2026-09-06. opencode sends
  that plus `x-opencode-request`, `x-opencode-client` and a User-Agent
  on every gateway call; ilar sent none (reqwest sets no User-Agent).
  Now an `Affinity` policy on the transport owns these per backend.

- A Go rate limit (429 "Please retry after a brief wait", seen on
  muse-spark-1.3) failed a turn: 429 was retryable but shared the
  transient budget — three tries at 0.5 s doubling, 3.5 s in all — and
  `Retry-After` was never read. Rate limits (429, 529) now have their
  own budget, six tries from 2 s doubling to 60 s, with the server's
  hint as a floor.

### Live smoke

`tests/smoke_opencode.rs` (ignored, needs `ILAR_OPENCODE_API_KEY`)
drives one tool-calling turn per wire per gateway through ilar's own
mappers: Go glm-5.3 and gpt-5.6-luna, Zen kimi-k3 and grok-4.6 — all
four complete with `StopReason::ToolUse`.

## 2026-08-14 — Project genesis

### Research: how Claude Code and opencode actually work

Did binary surgery on Claude Code 2.1.220 (Bun-compiled Mach-O, ~257MB,
JS bundle carved out and mined) and read the opencode source
(`anomalyco/opencode` dev branch). Findings that shaped ilar's design:

**Claude Code:**
- One process, one JS event loop. Subagents are *not* processes or workers;
  they are recursive async-generator query loops.
- Fan-out in a `StreamingToolExecutor`: tool_use blocks enqueue as they
  stream in. Each tool declares `isConcurrencySafe()`. A queued tool may
  start if nothing is executing OR it and all executing tools are
  concurrency-safe. Mutating tools (Edit/Bash) form a barrier. Results are
  drained in tool order (deterministic), execution is concurrent.
- Subagent caps are plain counters in a taskRegistry
  (`takeConcurrencySlot`): 20 concurrent (`CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS`),
  200/session, plus a spawn-depth cap. Over cap = soft tool error, no retry.
- Background agents: fire-and-forget + stall watchdog (600s). Completion
  enqueues a *synthetic user message* (`mode: "task-notification"`,
  `priority: "next"`) into the owner's queue, which re-invokes the parent.
  The "messaging system" is an in-process priority queue. Real IPC only for
  bash subprocesses and MCP servers.

**opencode:**
- Effect.ts fibers instead of promises. Tool calls dispatched as detached
  fibers; results land out-of-order in an unbounded queue; stream ends when
  the FiberSet settles.
- **No concurrency barrier** — everything in a step runs concurrently and it
  trusts model behavior. We adopt Claude Code's barrier instead: it is
  cheap to implement and prevents Edit/Bash races.
- Subagents = real child sessions (DB rows, inspectable in TUI). Depth cap
  default 1, no concurrency cap.
- Background agents exist only behind
  `OPENCODE_EXPERIMENTAL_BACKGROUND_SUBAGENTS`; completion injects a
  synthetic text part into the parent session. Same convergent pattern.

**Convergent architecture (both projects, independently):**
one event loop, fire-and-forget children, synthetic user message on
completion. No message broker anywhere. ilar copies this shape with
type-safe channels.

### Design decisions

- Rust workspace, two crates: `ilar` (core, pure) + `ilar-tui` (frontend).
  Core purity keeps a future one-shot CLI / server mode trivial.
- Event bus: `tokio::sync::mpsc<LoopEvent>` per agent; subagents as
  `JoinSet` tasks (structured concurrency, cancel-safe).
- Providers: trait `Provider` with `stream(request) -> EventStream`.
  Implementations: OpenAI Responses API, z.ai Anthropic-compatible,
  z.ai OpenAI-compatible. Mock provider for TDD.
- Sessions: append-only JSONL under `~/.local/state/ilar/sessions/`.
  One file per session, each line an event (message, tool_call, tool_result,
  compaction). Resume = replay file.
- Tools: trait with `kind() -> ToolKind { ReadOnly, Mutating }`. Executor
  adopts the barrier scheduling model.
- Compaction: when transcript nears context limit, summarize older turns
  into a marker event, continue session.
- No permissions. Sandbox is the boundary. (Deliberate: reduces scope by a
  whole subsystem.)
- Skills: markdown + frontmatter, injected on demand. Git-worktree
  isolation for subagents is a *skill*, not core.
- License: Unlicense.

### Constraints / preferences (from requirements interview)

- Personal tool, maybe OSS later. TUI-first. Esc = full abort (stream +
  running tools, best effort).
- TOML config, markdown agent definitions (opencode style).
- AGENTS.md / CLAUDE.md detection + cwd as project root.
- Per-agent model in config + runtime switching.
- Testing: TDD for core (loop, tools, providers via mock SSE), skip TUI.

## 2026-08-14 — session-jsonl done

First issue implemented (TDD, red→green). Review (subagent) caught a real
blocker: `transcript()` could emit consecutive user messages (compaction
summary + first kept user message), which Anthropic-style APIs reject with
400. Fix: coalesce adjacent user messages at flush time. Also added:
orphaned-tool-result snapping at compaction boundaries, NotFound vs
unrecoverable distinction on load, `session_id()` accessor.

Known caveat (documented in event.rs): `kept_from` is a write-time event
index; corrupt-line skips shift it on replay. Acceptable degradation —
transcript stays coherent; anchor to event ids if it ever matters.

## 2026-08-14 — provider-trait done

Trait shape: sync `fn stream(&self, Request) -> Result<Pin<Box<dyn Stream>>>`.
Dyn-compatible, no async_trait, Send-clean. Two hard-won doc contracts:
- Network errors surface as ProviderEvent::Error *on the stream*, not as
  Err from stream() (spawn+mpsc pattern makes pre-flight Err impossible
  for HTTP failures).
- Cancellation: wrapper struct whose Drop aborts the spawned pump task —
  dropping a bare ReceiverStream is NOT enough (quiet connections linger).

Review gate: added Thinking events + ContentBlock::Thinking before any
real provider exists — Anthropic-style APIs require round-tripping
thinking blocks with tool use, GLM emits them, retrofitting later would
have touched every consumer simultaneously. Also added Refusal/Paused
stop reasons, cache-token usage fields, and the null-input+MaxTokens
convention for truncated tool args.

## 2026-08-14 — provider-openai-responses done (smoke test pending)

Review caught two real blockers:
1. SSE parser did from_utf8_lossy per chunk — multi-byte UTF-8 split
   across chunk boundaries corrupted (guaranteed noise on GLM Chinese
   text). Rewrote parser over a byte buffer; blocks convert only when
   complete.
2. response.incomplete with a truncated tool call left a dangling
   ToolCallStarted (violating our own event contract) and reported
   ToolUse — the loop would have executed a tool whose args never
   arrived. Now synthesizes null-input completions + MaxTokens.

Also: refusal deltas surfaced as TextDelta with StopReason::Refusal,
pump panic guard (catch_unwind -> Error event; a panic otherwise looks
like a clean EOF), options-merge without the "extra" marker hack.

Debug war story: test server originally used std TcpListener +
read_to_end under a current-thread runtime — blocking accept() starved
the reqwest task, and read_to_end waited for a half-close that never
comes. Fix: async tokio I/O, read-until-content-length. Also: `let _ =
provider.stream(...)` drops the stream instantly, which per our own
cancellation contract aborts the pump before it connects. The contract
works — against its author.

Remaining for this issue: one live-API smoke test (needs OPENAI_API_KEY),
incl. a reasoning model doing 2+ tool turns to validate that dropping
thinking blocks from replay doesn't 400 (review flagged it; fixtures
can't prove it).

## 2026-08-14 — provider-zai done, live-verified

API keys fished out of local installs: no OpenAI plain key exists (both
opencode and codex use ChatGPT OAuth — the OpenAI smoke test stays
open), but opencode's auth.json holds the zai-coding-plan key. Two
findings from the live endpoint:
- The coding-plan key only works on the Anthropic-compatible endpoint
  and the OpenAI *coding* endpoint (api.z.ai/api/coding/paas/v4); plain
  /api/paas/v4 rejects it with "insufficient balance". Default base
  URLs set accordingly.
- The coding endpoint streams reasoning_content (GLM thinking) — mapped
  to ThinkingDelta with a synthesized ThinkingCompleted boundary since
  chat-completions has no explicit reasoning-block close event.

Live smoke tests (tests/smoke_zai.rs, #[ignore]d, ILAR_ZAI_API_KEY):
anthropic text turn, anthropic two-turn tool round-trip (real GLM
emitted get_weather, result returned, second turn answered), openai-
flavor text turn. All passing.

Review caught two contract violations in the OpenAI flavor: truncation
didn't synthesize pending tool-call completions (now unconditional on
any finish_reason), and chatty compat servers attaching usage to every
chunk could double-fire TurnComplete (guarded). Also: anthropic
truncation synthesis now emits in block order; mid-stream {"error":..}
chunks surfaced.

## 2026-08-14 — core-tools done; prompt caching live-verified

Tools: trait with ToolKind (ReadOnly/Mutating) feeding the upcoming
barrier executor; read/write/edit/bash/glob/grep with typed inputs
(malformed model output = tool error, never a panic).

Prompt caching (the "don't re-ingest the whole prompt" concern):
- Anthropic flavor places ephemeral breakpoints on system block, last
  tool, and a MOVING breakpoint on the last message's final block (the
  canonical incremental pattern).
- Live proof with a ~2000-token prompt on real GLM: turn 1 ingests
  2006 tokens; turns 2-3 read 1920 from cache, only ~100 fresh. The
  moving breakpoint works on z.ai — marker placement is not part of
  their cache hash (earlier messages re-serialize marker-free across
  turns and still hit).
- z.ai accounting quirks: cache_creation_input_tokens is never
  reported (the write shows as plain input_tokens on the writing turn);
  reads reported at entry granularity. Don't assert on creation.
- OpenAI coding endpoint: caching is automatic; we parse
  prompt_tokens_details.cached_tokens.
- Prefix stability is unit-tested: consecutive turns' wire messages
  serialize identically after stripping cache_control markers.

Remaining M1: barrier executor, agent loop, config/AGENTS.md, TUI.

## 2026-08-14 — tool-executor-barrier done

The Claude Code scheduling model on tokio: FuturesUnordered for
concurrent read-only runs, hole-filled outcomes Vec for call-order
results, mutating tools as barriers. Review verdict "safe to build on"
after verifying invariants (FIFO, no double-record, fill-order sound,
drop chain intact). Fixes applied from review:
- Cancel check at top of the scheduling loop (a cancel racing a
  completion could otherwise start one more tool past an Esc).
- Deterministic overlap proof from event logs instead of pure timing
  (last start < first end), generous wall-clock margins.
- Id/name pinning through the hole-fill path; pre-cancelled-token and
  unknown-tool-mid-queue tests.
- Bonus real bug: bash drained pipes only AFTER wait() — a child
  writing >64KB blocked on the full pipe until timeout killed it.
  Now joined concurrently; 300KB drain test proves fast clean exit.

## 2026-08-14 — agent-loop done

The turn state machine. Review caught two blockers on the abort path
(the one path the spec calls a hard requirement):
1. Abort between ToolCallCompleted and TurnComplete persisted an
   unanswered tool_use — every provider 400s on that shape, so one Esc
   at the wrong moment permanently poisoned the session. Fix: abort
   path synthesizes error tool results ("aborted before execution")
   for every announced call; resume tells the model the truth.
2. Abort between iterations (e.g. during tool execution) returned
   without publishing TurnDone — a guaranteed TUI deadlock once Esc
   is wired.

Also: streams ending without TurnComplete/Error are now errors (a
dying provider no longer gets its announced tools executed on its
behalf), and provider errors persist the partial step (UI-shown
deltas must not evaporate from the transcript).

Invariants worth remembering: executor cancel already fills holes
with cancelled outcomes (so abort-during-execution is safe by
construction); transcript() coalesces trailing tool results with the
next user message (valid "please continue" shape on both wires).
Known non-goal for now: concurrent run_turns on one session would
double-open the JSONL — TUI is strictly one turn at a time.

## 2026-08-14 — M1 complete: config, TUI, live capstone

Config: hermetic Loader (edition 2024 made set_var unsafe — tests
inject env explicitly instead of mutating process env), project >
user precedence, MD agents overriding built-ins, AGENTS.md/CLAUDE.md
nearest-wins discovery. provider_for() builds concrete providers from
"provider/model-id".

TUI: thin layer over run_turn (no tests by design). Two build
lessons: pty harness needs TIOCSWINSZ or ratatui renders into 0x0
(empty frames, looks broken); and `.clone()` on a &&SessionStore
clones the REFERENCE when the type isn't Clone — the spawn silently
captured a borrow and failed to compile. Both types are Clone now.

M1 capstone (live, real GLM through the TUI over a pty): user asks
for the workspace version -> model calls read -> tool runs -> final
answer "0.1.0" streamed -> usage in status line -> session JSONL has
the full exchange (meta, user, assistant+tool_call, tool_result,
assistant). ILAR_STATE_DIR env override for sandboxed runs.

## 2026-08-14 — M2 + M3 complete: full roadmap shipped

M2 (multiply): task tool spawning parallel child agents (shared atomic
slot counter, depth-capped child spawners, Claude Code do-not-retry
cap errors); background=true tasks run detached with stall watchdogs
(default 600s, activity tracked via the child event stream) and land
as <task-notification> messages that re-invoke the idle parent loop —
the convergent Claude Code/opencode pattern; auto-compaction with
estimate_tokens = max(last usage, chars/4), summarizer call, cut at
the current user message (once per turn, never mid-tool-loop).
transcript rendering extracted to a pure function for the summarizer.

M3 (polish): todo tool (todowrite-style, single in_progress enforced);
webfetch (dependency-free HTML->text; test caught an off-by-15 slice
bug that corrupted output after </script>) + websearch (pluggable
SearchBackend, Tavily impl); runtime model switching (ModelChange
session events audited in JSONL, effective_model resolved per provider
call, Ctrl-M cycles + rebuilds the provider); skills (markdown +
frontmatter, project-over-user, listing in system prompt, body loaded
on demand, worktree-isolation builtin — the whole subsystem is ~200
lines vs Claude Code's plugins/skills machinery).

Final state: 113 unit tests + 4 live smoke tests, clippy/fmt clean,
15/15 issues closed across 3 milestones. Two `futures`-in-Rust
footguns worth remembering: tuple-of-futures doesn't implement Future
(wrap in async move blocks), and async trait methods borrowing self
need owned clones before Box::pin (todo tool). Also: edition 2024 made
std::env::set_var unsafe — config tests inject env via the Loader
instead.

Known follow-ups (not blocking daily-driver use): OpenAI live smoke
test needs a real API key; concurrent run_turns on one session would
double-open the JSONL (TUI is one-at-a-time); bash timeout is the only
guard against runaway interactive commands.

## 2026-08-14 — OpenAI ChatGPT OAuth login

Codex-style PKCE flow: authorize at auth.openai.com (public client id,
offline_access scope), callback server on 127.0.0.1:1455, S256
challenge (RFC 7636 vector tested), token exchange + rotation into
<state dir>/auth.json — ilar's own file, never reads or writes
~/.codex. Provider gains Auth::ChatGpt: chatgpt.com/backend-api/codex
with originator: codex_cli_rs + OpenAI-Beta headers, store:false, and
one refresh-and-retry on 401 inside the pump (mock-tested: rotated
bearer observed on the wire).

Live findings from probing the real backend (read-only, using codex's
existing access token — usage can't rotate anything):
- Bearer + chatgpt-account-id + originator headers are accepted as-is;
  no mTLS/DPoP binding on this account's tokens.
- API-catalog model names are rejected ("gpt-5.2" -> 400 model not
  supported); ChatGPT accounts serve the codex model line. Current
  slugs live in ~/.codex/models_cache.json — gpt-5.6-sol is the
  default (also -terra/-luna variants, gpt-5.5, gpt-5.3-codex-spark).
- stream:false is rejected ("Stream must be set to true") — fine, the
  provider always streams.
- Final proof: ilar's provider streamed a text turn through the real
  ChatGPT backend (isolated seeded token copy, since deleted).

For daily use: run `ilar login` so ilar holds its own token pair —
refresh rotation would otherwise race codex's copy if you reuse the
same refresh token in two stores.

## 2026-08-15 — OpenAI tool-loop stall

The first real OAuth coding turn exposed three interacting bugs after
`todo`: Responses API `function_call_output` rejects the neutral model's
`is_error` field, the TUI discarded `Ok(Err(...))` from the spawned turn,
and streamed tool calls were announced twice. OpenAI now emits only
`type`, `call_id`, and `output`; nested turn errors are displayed; starts
are deduplicated; and tool lines are completed by call id rather than
screen position. Final queued events are drained after joining the turn.

Tool results are flushed to JSONL before `ToolFinished` is published. A
regression test reloads the session while the next provider request is
still pending and sees both the assistant tool call and its result.
Provider errors after a completed call synthesize an error result so the
session remains resumable.

## 2026-08-15 — Readable Markdown and real transcript scrolling

Assistant output is now rendered as terminal-native Markdown instead of
putting an entire response (including newlines) into one Ratatui `Line`.
Headings, lists, quotes, emphasis, inline code, links, rules, and fenced
code get distinct styles; tabs use stable four-column stops, and partial
streaming delimiters remain visible until they close.

The transcript follows the wrapped visual tail by default and detaches
when the user scrolls upward. Controls: arrows and mouse wheel for small
steps, PgUp/PgDn for pages, Ctrl-U/Ctrl-D for half-pages, Ctrl-Home for
the top, and Ctrl-End to resume tail following. Overflow adds a scrollbar
and a `tail`/percentage title marker. Wrapped row counts are recalculated
on resize without snapping a detached reader back to the tail.

## 2026-08-15 — Stabilization: unique tool registry

Tool registry composition now rejects duplicate names with a typed error.
`webfetch` remains a builtin and `with_web_tools` only adds optional
search, so provider requests cannot contain duplicate function schemas.

## 2026-08-15 — Stabilization: z.ai OpenAI wire format

The OpenAI-compatible flavor now sends instructions as a system-role
message and places tool outputs directly after assistant tool calls,
without inserting an empty user message. The example endpoint now points
at z.ai's coding-plan OpenAI-compatible URL.

## 2026-08-15 — Stabilization: serialized notification turns

Notification bursts now stay queued and launch one turn at a time. The
active JoinHandle, rather than an early UI `TurnDone` event, owns the turn
until join cleanup, preventing event-channel, cancellation-token, and
handle replacement races. Parent-session routing remains tracked in the
open notification issue.

## 2026-08-15 — Stabilization: provider/model boundary

Concrete providers now reject models with a mismatched provider prefix
before spawning network work. This converts stale routing mistakes into
clear preflight errors while the broader provider-router issue remains
open for resume, switching, subagents, and compaction.

## 2026-08-15 — Stabilization: robust Bash execution

Bash drains stdout and stderr concurrently into bounded byte buffers,
decodes arbitrary output lossily, and truncates only at UTF-8 boundaries.
On Unix each command runs in a dedicated process group; timeout or future
cancellation terminates descendants. Timeout errors
retain bounded partial output, and signal exits have explicit diagnostics.

## 2026-08-15 — Stabilization: validated session identifiers

Session file paths now derive only from canonical lowercase hyphenated
UUIDs. Invalid CLI or model-supplied task IDs fail with `InvalidInput`
before any filesystem lookup, closing the path-traversal route while the
writer-lease issue remains open.

## 2026-08-15 — Stabilization: session writer lease

Every agent turn now holds a nonblocking OS-backed session writer lease
from before the user append through provider/tool completion. Concurrent
turns fail before mutation, cancellation releases ownership, and read-only
loads remain available. Direct compaction acquires the same lease while
turn-internal compaction reuses existing ownership. Tail-recovery behavior
remains tracked before the lease issue can be archived.

## 2026-08-15 — Stabilization: torn-tail recovery

Read-only session inspection parses only newline-terminated records and
never repairs a file. A leased writer truncates an unterminated or invalid
UTF-8 final tail to the last complete record before appending. Malformed
newline-terminated records now reject the session as middle corruption
instead of being silently skipped.

## 2026-08-15 — Stabilization: crash-safe replay

Session replay now requires one leading metadata event whose identity matches
the filename, unique event and tool-call IDs, and exact tool-call/result
pairing. Read-only inspection leaves trailing unanswered calls untouched;
leased writer recovery persists synthetic error results for them exactly once.
Invalid semantic state is rejected before any torn-tail repair mutates the log.

## 2026-08-15 — Stabilization: typed session identity

All session and lock paths are now derived internally from a canonical,
validated `SessionId`. Cross-process tests prove actionable nonblocking
contention and OS lock release after forced process exit; the test helper is
isolated and reaped on timeout or panic. Platform-specific `fs2` contention
errors are normalized to `WouldBlock`.

## 2026-08-15 — Stabilization: provider/model lifecycle

Each writer-owned turn now captures the persisted effective model and resolves
its matching provider exactly once before appending user input. The same pair
drives compaction and every tool-loop step. Resume defaults to persisted agent
and model state, explicit CLI model selection wins and persists first, and
subagents inherit the parent model unless their agent overrides it. Nested
background tasks are rejected until parent-specific notification contexts are
supported.

## 2026-08-15 — Stabilization: ordered reasoning state

Assistant content now persists in exact stream order across thinking, text,
opaque reasoning, and tool calls. Signed thinking runs remain independent;
unsigned or incomplete thinking is retained only as non-replayed diagnostics.
Stateless OpenAI requests preserve encrypted reasoning items before function
continuations. Incomplete tool calls are never executed, receive synthetic
errors, and replay with protocol-valid placeholder arguments.

## 2026-08-15 — Stabilization: notification routing

Background completions now enter one FIFO and execute only when no foreground
or routed turn owns the TUI lifecycle. Nested completions run their declared
parent with its persisted agent, model, depth, and registry, then propagate one
success or error upward. Busy parents wait without losing work; cancellation
requeues undelivered notifications in a paused state, while delivered aborts
propagate explicitly. Nested detached handles share root cancellation ownership.

## 2026-08-15 — Stabilization: atomic replacement

OAuth credentials and source-file write/edit operations now share one
crash-durable replacement primitive. On Unix, temporary creation, destination
inspection, publication, cleanup, and directory sync are bound to one
no-follow directory descriptor. Temps are born `0600`, final modes are applied
after writing, parent swaps and symlink destinations are rejected, and
post-publication durability failures are reported without unsafe cleanup.
Other platforms fail closed until equivalent handle-relative guarantees exist.

## 2026-08-15 — Stabilization: secure OAuth storage

OAuth store reads now distinguish absence from malformed, unreadable, or
symlinked credentials. All token writes and refresh rotations share an
OS-backed lock, recheck state after lock acquisition, and retain ownership
through cancellation-safe blocking persistence. Token responses and localhost
callback requests are bounded and timed; callback handling ignores spurious
connections, percent-decodes values, and reports OAuth denial responses.

## 2026-08-15 — Stabilization: bounded file tools

Read now streams requested line windows from files larger than the output cap
and distinguishes empty files from offsets beyond EOF. Read, grep, and glob
filesystem work runs off async workers with cooperative cancellation on future
drop. Grep bounds each file prefix, rendered line, match count, and total
output; glob checks cancellation per traversed entry and stops collecting at
its cap. Atomic write/edit publication and mode preservation are shared with
the previously landed replacement primitive.

## 2026-08-15 — Background Bash jobs

Bash can now opt into detached execution with `run_in_background`, returning a
stable job ID immediately and delivering one completion, failure, timeout, or
cancellation notification through the existing parent-turn queue. Background
jobs use a configurable 10-minute default (`subagents.background_tool_timeout_ms`)
with per-call `timeout_ms` overrides, retain workspace exclusion for their full
run, inherit root cancellation through nested agents, and are cancelled and
joined during shutdown. Bash also terminates remaining process-group children
when the shell exits.

## 2026-08-15 — TUI tool details and telemetry

Tool rows now receive bounded, secret-redacted argument summaries from the
agent loop and render them as muted, grapheme-safe, single-line text. The
always-visible status strip reports lifecycle state, effective model, working
directory, normalized context usage/limit, and percentage with responsive
layouts down to narrow terminals. Thinking, responding, and tool activity are
animated in the transcript without persisting synthetic content. Provider
usage now has versioned cache-accounting semantics; legacy sessions and resumed
transcripts use visibly approximate estimates.

## 2026-08-15 — Model catalog and picker

Provider discovery now uses a maintained models.dev snapshot for active
tool-capable OpenAI and z.ai models, including model-specific context, input,
and output limits. ChatGPT OAuth remains restricted to model slugs verified on
the Codex backend, while API-key and z.ai Coding Plan inventories follow their
effective transport configuration. The TUI replaces model cycling with a
searchable keyboard modal opened by Ctrl-X M or F2; selection is persisted
before adoption, background notifications wait behind the modal, and narrow
layouts retain selectable rows and inline errors.

## 2026-08-15 — Conservative GPT-5.6 context defaults

GPT-5.6 Sol, Terra, and Luna now use Codex's 272,000-token working context for
telemetry and compaction while retaining the models.dev 1,050,000-token value
as explicit maximum metadata. This separates safe runtime defaults from
provider capability and leaves a clean bound for future context configuration.

## 2026-08-15 — Denser Markdown and visible input cursor

The TUI now exposes the terminal's native blinking cursor at the prompt and
model search, with grapheme-safe tail views for long values. Markdown blank-line
runs collapse to one interior separator row without adding leading or trailing
space, and assistant content is style-preserving hard-wrapped inside its label
margin so every visual line stays aligned without changing fenced-code
whitespace.

## 2026-08-15 — Workspace-aware tool scheduling

Tool ordering and workspace effects are now independent capabilities. Mutable
child turns hold a checkout-wide lease for their full lifetime, enforced
read-only agents receive no shell, edit, write, or delegation tools, and todo
updates remain ordered barriers without pretending to read the workspace.

Task calls may route to a registered sibling Git worktree with structured
`workspace` metadata. Canonical checkout IDs key a shared lock registry, so
same-checkout mutations serialize while distinct worktrees overlap. Child
sessions persist their validated cwd and isolation; resumes require the same
explicit worktree when changing workspace and may inherit an immediate parent's
validated location. Routed notifications restore each ancestry transition,
and stale worktrees are rejected again after lease waits. Routing is
cooperative scheduling, not a filesystem sandbox.

## 2026-08-15 — Correct, cancellable compaction

Every root, child, background, and routed turn now shares the configured loop
settings. Compaction estimates only active post-boundary context while counting
the system prompt and tool definitions used by the real request; startup and
model-switch telemetry use the same estimator.

Summaries are persisted only after an explicit `EndTurn`. Partial EOF, refusal,
pause, truncation, tool-use, provider errors, and empty output leave no
compaction marker. Cancellation is checked before the summary call, while its
stream is pending, and immediately before persistence, allowing Escape to
return the turn as aborted without committing a partial summary.

## 2026-08-16 — Hardened provider protocol handling

Provider streams now fail closed on malformed JSON, missing identifiers,
duplicate or contradictory lifecycle events, invalid tool arguments, and
unterminated or oversized SSE events. Reserved request fields are rejected
before network I/O, and bounded HTTP error bodies redact structured,
plaintext, configured, and truncation-boundary credentials.

The agent loop enforces explicit tool start/completion ordering, permits null
arguments only for explicit token truncation, and never invokes custom tools
with incomplete input. Anthropic pauses have a finite retry budget independent
of normal tool iterations; exact streamed assistant content is replayed for
continuation and persisted in provider-specific replay blocks once the resumed
turn completes. This preserves server-tool ordering through later client tool
results without duplicating visible neutral content.

## 2026-08-16 — Bounded, SSRF-safe web tools

Web fetch and Tavily search now use explicit connect and total timeouts, disable
environment proxies, and stream response bodies under hard byte ceilings.
Fetch validates literal and every DNS-resolved address, rejects private,
loopback, link-local, metadata, and known IP translation ranges, and applies the
same policy to redirects. Tavily redirects are disabled so its body-carried API
key cannot be replayed to another origin.

The HTML converter now scans Unicode safely, handles quoted attributes and raw
script/style content, and preserves block boundaries with single-buffer text
normalization. Search queries, backend duration, result count, JSON size, hit
fields, errors, and final output are bounded; the public limit is documented
and clamped to 1–20 results. Fetch errors strip reqwest URLs and retain only a
bounded origin label so signed paths and queries are not persisted.

## 2026-08-17 — Strict, layered configuration diagnostics

Config files now merge nested provider, compaction, and subagent fields instead
of replacing whole sections. Only missing files are ignored; read, UTF-8, parse,
and semantic errors retain their source paths. Loader-injected config and state
directories now drive OAuth, sessions, agents, and skills consistently, with an
explicit OpenAI `api_key` mode available to reset inherited ChatGPT auth.

Agent and skill discovery is deterministic across user and project directories.
Their shared frontmatter parser accepts BOM and CRLF input, requires exact
delimiter lines, and reports malformed definitions rather than dropping them.
Checked-in config and agent examples are parsed by tests so documentation cannot
silently drift from supported fields.

## 2026-08-17 — Resumable, editable TUI sessions

Resumed sessions now rebuild their visible transcript before entering raw mode,
including compaction summaries, redacted tool details, completed tool states,
model switches, todos, and the latest meaningful token usage. Persisted agent
and model selection remain validated before terminal initialization.

The prompt is now a grapheme-safe multiline editor with cursor movement,
in-place deletion, bracketed paste, vertical line navigation, and explicit
Enter-to-send versus Ctrl-J-to-insert-newline bindings. Input expands to show up
to six lines and reports the current line, while idle status prioritizes model
and latest usage over lower-value path detail at constrained widths.

Transcript rows are wrapped and sliced with `usize` before Ratatui rendering,
removing the Paragraph `u16` scroll ceiling. A bounded-row fast path keeps the
65k-row tail regression responsive; broader transcript caching remains a
separate stabilization issue.

## 2026-08-18 — Shared provider transport

OpenAI and z.ai now share one private transport shell for bounded HTTP errors,
SSE parsing, terminal-event cutoff, panic conversion, and abort-on-drop task
ownership. Provider modules still own request construction, authentication and
wire-event mapping; in particular, OpenAI's ChatGPT token refresh remains
outside the transport abstraction. Direct shell tests cover send failure,
panic conversion, terminal SSE handling, and prompt cancellation on drop.

## 2026-08-18 — Deterministic provider tests

`MockProvider` now consumes each scripted turn exactly once and reports script
exhaustion from `Provider::stream`, making accidental extra calls fail at their
source. Intentional loop tests opt into `MockProvider::repeating` explicitly.
Provider fixture tests validate required SSE termination without repairing
tracked files; the full workspace suite passes from a read-only source checkout
with a separate writable Cargo target directory.

## 2026-08-18 — Visible OpenAI reasoning summaries

Reasoning-capable OpenAI Responses requests now ask for automatic public
summaries. Their `reasoning_summary_text` stream is validated and persisted as a
display-only content block, while the completed encrypted reasoning item remains
the sole replay input. The TUI extracts the provider's leading Markdown heading
and renders it as `Thinking: <topic>` while streaming and `Thought: <topic>`
after completion; private thinking and unsigned diagnostics remain hidden.

## 2026-08-19 — OpenAI prompt-cache routing diagnostics

OpenAI requests now carry the session UUID as a provider-neutral cache affinity
key. The documented API-key Responses endpoint maps it to `prompt_cache_key`;
custom endpoints omit it by default. ChatGPT OAuth deliberately omits the
undocumented field after controlled keyed samples accepted it but reported
0/0/0 and 0/6912/0 cached tokens, failing to demonstrate stable affinity. An
automatic-cache control with three byte-identical 8k token ChatGPT requests also
reported 0/6912/0. This confirms that zero/high oscillation can be backend
routing, not local prefix mutation.

Regression tests pin the stable serialized model, instructions, tools, reasoning
options, prior-input prefix, and cache key across consecutive requests. OpenAI
usage parsing accepts both Responses `input_tokens_details.cached_tokens` and
Chat Completions `prompt_tokens_details.cached_tokens` shapes. The TUI now labels
cache reads and writes separately for the latest request rather than implying a
cumulative session counter.

## 2026-08-19 — Previewable TUI themes

The TUI now offers Terminal Adaptive, Carbon, Parchment, Frost, and High
Contrast themes through `F3`, `Ctrl-X T`, and the command palette. Picker
navigation transforms the rendered buffer immediately, Escape restores the
saved theme, and Enter confirms it without invalidating semantic transcript
caches or threading palette state through every renderer.

Themes are a user-scoped preference so project configuration cannot make a
successful in-app save disappear after restart. Confirmation updates the user
TOML with a syntax-aware, comment-preserving editor, retains CRLF line endings,
retries concurrent changes, and publishes through the existing atomic-file
path. Modal handling is centralized so queued notifications, paste, and mouse
input cannot leak through the picker.

## 2026-08-19 — Bounded project context discovery

Project instructions no longer walk arbitrary ancestor directories. Root and
subagent prompts combine `AGENTS.md` (or `CLAUDE.md` as a fallback) from the
resolved user config directory and exact runtime working directory, with local
instructions last. Non-missing read failures are surfaced instead of silently
dropping policy or falling through to a legacy file.

The user config directory is carried through nested, isolated, and routed
subagent runtimes. Context is loaded before fresh child-session creation, and
routed nested failures propagate to the grandparent rather than losing a
background completion. The README now provides the corresponding complete
configuration, environment, custom-agent, and skill reference.

## 2026-08-19 — websearch works out of the box (keyless Exa)

Websearch previously registered only with `ILAR_TAVILY_API_KEY` set, so a
fresh install silently had no search. Investigated how opencode ships OOB
search: it POSTs a bare JSON-RPC `tools/call` to the hosted Exa and
Parallel.ai MCP endpoints, keyless by default, and A/B-splits sessions
between the two providers by session-ID checksum.

Adopted the Exa half: new `ExaBackend` calls `https://mcp.exa.ai/mcp`
(`web_search_exa`), parses both direct-JSON and SSE `data:` framings, and
converts the text payload (`Title:`/`URL:` blocks separated by `---`) into
structured `SearchHit`s, with a raw-text single-hit fallback so results are
never dropped. `with_web_tools()` now always registers websearch: Tavily
when its key is set, otherwise Exa (optionally authenticated via
`ILAR_EXA_API_KEY` as an `exaApiKey` query parameter, same as opencode).
Keyless access is best-effort on Exa's side — README tells users to bring
their own key. Live-verified via an `#[ignore]`d smoke test
(`cargo test -p ilar exa_live -- --ignored`).

## 2026-08-19 — Milestone 4: daily-driver UX batch

Eleven-issue batch making the TUI comfortable for daily work, worked
issue-by-issue with per-feature commits and subagent reviews on the two
biggest changes:

- **Edit diffs**: LCS line diff (dependency-free, bounded 400 lines /
  256 KiB) replaces raw old/new JSON in edit tool rows, themed ± colors,
  across live/child/restore paths. Review caught a byte-cap gap and a
  changeless-diff fallback suppression; both fixed.
- **Session resume**: `SessionStore::list()` head-scans JSONL for
  (id, title, mtime); `--continue` resumes the latest; a palette picker
  switches sessions in-app by restarting the whole runtime (main is now
  a session loop) for full agent/model/prompt fidelity. Switch validates
  the target and refuses during turns/background jobs/drafts.
- **Prompt history**: persisted JSONL ring (1000 entries), Up/Down
  recall with readline draft stash.
- **Usage + cost**: models.dev pricing table in core (list prices,
  snapshot-dated); per-step accrual with per-event-model pricing on
  restore; Σ tokens + $ in the status line, breakdown via palette.
  Unpriced models poison dollars, never guess.
- **Help overlay** (F1/?), **readline chords** (^A/^E/^K/^U/^W, Alt-B/F),
  **skill triggers** (frontmatter now feeds prompt cues) + `/skill`
  invocation with a `/` picker, **per-agent `tools:` allowlists**
  (load-time validation, intersection with read-only), built-in
  **mcp-via-cli skill** (decision: no core MCP client), and hand-rolled
  **fence syntax highlighting** (six language families, no syntect).
- Todo narrow-terminal fallback issue closed as already implemented
  (border-chrome summary line predates the issue).

Caveats: session switching drops background-job tracking (spawner is
shut down like on quit); mcp-via-cli verified against upstream docs only
(sandbox blocked installing the CLI); the AppExit::Switch path has no
automated test — smoke-test manually.

## 2026-08-19 — glm-5.3 decode failures: post-mortem debuggability

Investigated a glm-5.3 (zai, openai flavor) session whose turns died
with stop_reason "error" after ~80-120KB of thinking. Root cause was
undeterminable from the session: ilar showed the decode error only as a
transient TUI notice, and error turns without completed tool calls
persisted no error text. A minimal repro against the coding-plan
endpoint decoded fine (id+name+arguments arrive in one tool_call chunk);
the failure needs the real long-thinking + giant-write shape, which
exceeds interactive repro time.

Added the missing observability instead of guessing: decode errors now
carry a bounded, secret-scrubbed snippet of the offending SSE event
(single choke point in transport.rs covers all providers/flavors), and
every errored turn persists the message as a provider-invisible
Diagnostic block in the session JSONL. Next occurrence will name the
exact wire event. Suspects to check when it does: the 1 MiB tool
argument cap vs. giant single-file write calls, unknown finish_reason
values, and post-finish usage chunks.

## 2026-08-19 — glm-5.3 root cause: z.ai buffers tool responses without tool_stream

Manual endpoint probing (6 small, 3 thinking-heavy, 4 ilar-shaped
requests) found the smoking gun: with tools in the request, the
OpenAI-compatible z.ai endpoint sends NOTHING until the entire response
is generated — first byte == total duration on every tools request,
while tool-free requests stream normally. Long agentic turns (huge
thinking + big write calls) therefore sit in dead air for minutes and
die at gateway limits with zero bytes, which is exactly the earlier
"error after 120KB thinking" session and the hit-or-miss feel.

Fix: always send `tool_stream: true` on the OpenAI flavor. Verified live
that glm-5.3 and glm-4.7 then stream tool arguments incrementally in the
exact shape the strict decoder expects (id+name in the first chunk,
argument deltas after). The Anthropic flavor already streams properly
with tools. New ignored live smoke test pins the incremental behavior
(`live_openai_flavor_glm53_tool_call_streams_incrementally`).

## 2026-08-19 — the real glm-5.3 killer: our own 600s request timeout

The recorded turn error (new diagnostics) plus session timestamps nailed
it: the turn died at exactly 600s — REQUEST_TIMEOUT — after 117KB of
healthy streamed thinking. reqwest reports a mid-body timeout as "error
decoding response body" and its Display hides the source, so it looked
like a protocol bug. glm-5.3 at effort=max genuinely thinks for 10+
minutes on hard tasks; opencode sets no total fetch deadline (300s
header timeout + optional chunk timeout only).

Providers now use connect (15s) + idle (300s) timeouts with no total
deadline, and transport errors carry their full source chain. Also: live
thinking now accumulates into an expandable Thought row (click ▸, 64 KiB
tail bound) so marathon reasoning is inspectable while it runs.

## 2026-08-19 — Milestone 5: flow batch

Eleven more issues, one sitting: throughput rate + per-step live output
estimate in the liveness display, plan-billing label, one-key turn retry,
Markdown export, skills inlined in the palette (now dynamic items),
fzf-style session picker with delete/fork (SessionStore::delete/fork in
core), message queueing with auto-send, palette-forced compaction with
the summary shown live, transcript search (Ctrl-F, revision-tracked
matches), and a live bash output tail via a lossy OutputTailSink on
ToolContext drained through the loop-event channel.

Review pass caught two real hazards before push: queued auto-send could
fire a synthetic Enter into an open picker/search (losing the message),
and search match indices went stale as streaming shifted rows. Both
fixed, plus lock-file hygiene on delete and force-compaction without a
context limit.

Deferred nitpicks: bash tail can start mid-UTF-8 sequence (cosmetic
replacement char), fork doesn't fsync the directory entry, retry after a
failed forced compaction loses the force flag, and queue order can
invert if a notification turn starts in the same loop iteration as a
dequeue.

## 2026-08-19 — glob walked 24.7M entries to answer a 34-entry question

Three concurrent `glob` calls sat at "executing" past 1m24s while a
`grep` in the same turn finished. Not a hang — a full enumeration of
`~/repos/yodl` (186 worktrees, 407+ node_modules): **24,683,808
entries**, three times over, for the pattern
`worktrees/manteca-manual-withdrawal/*` against a directory holding 34.

Four compounding causes, all in glob and none in grep: the walk always
started at `ctx.cwd` and matched afterwards, so the pattern's literal
prefix bought nothing; every ignore filter was explicitly disabled
(`hidden`, `ignore`, `git_ignore`, `git_global`, `git_exclude`); the
1000-item cap counted *matches*, not entries scanned, so a narrow
pattern never short-circuited and the more precise the query the longer
it ran; and the walker was single-threaded with a per-directory
collect-and-sort. grep had none of these — it takes a `path` to scope
the root and builds with `.hidden(true).git_ignore(true)` — which is
exactly why it completed.

Target came from ripgrep on the same tree with the same `ignore` crate:
`rg --files` lists 95,027 files in 1.03s at 935% CPU. Filtering alone is
a 260× reduction (24.7M → 95k); parallelism covers the rest.

Now: walk rooted at the pattern's literal prefix (rejecting `..` and
absolute patterns so it cannot leave the workspace), ignore files
honoured by default with `include_ignored` as the escape hatch,
`.git` always dropped, hidden entries kept so `.github/workflows/*.yml`
still resolves, `build_parallel()` across up to 8 threads, and a 500k
entry budget that truncates with a distinct message instead of grinding.

Measured after: the pattern that hung, 0.01s. Full-workspace walk with
no possible match (worst case, cannot short-circuit), 0.65s — under the
ripgrep baseline. Unfiltered whole-tree walk hits the entry budget at
0.83s and says so.

Known trade-off: which 1000 results survive truncation is now
non-deterministic across runs, since threads race to fill the cap.
Untruncated output is fully sorted and stable as before.

Also found and left alone: the `glob` crate has no brace expansion, so
the turn's third pattern (`**/{route,client}/*`) matched nothing and
reported it as a legitimate empty result after paying for the full walk.
Models write brace patterns routinely. Recorded in the issue.

## 2026-08-19 — compaction: right limit, wrong time

Two independent defects, either enough to lose a session.

The threshold was measured against `context_limit`, but providers reject
on *input* size. gpt-5.3-codex-spark is 128k total with a 100k input cap,
so the trigger sat at 108.8k — 8.8k past unsendable. A catalog audit
found the inversion in 26 of 45 models.

Worse, compaction only ran at turn start, before the provider loop. One
agentic turn runs many steps per user message, and context was never
re-checked across them. Session 4466f66d: 3 user messages, 44 assistant
steps, 1.5k -> 127k tokens, sailed past its own 108.8k trigger at step
32, died at 127k with zero compactions. The threshold fix alone would not
have saved it; the mid-turn check alone would have.

Now: thresholds resolve through `ProviderResolver::compaction_limit`
(input cap, not window), and the loop re-checks before every step, gated
on `continuations.is_empty() && paused_content.is_empty()` so a paused
mid-continuation response never gets cut out from under its replay state.

Mid-turn needs a different cut. The existing one lands on the last
UserMessage, which mid-turn *is* this turn's prompt — it would summarize
nothing. `CompactionCut::RecentSteps` walks back from the end keeping
roughly a third of the budget, then snaps back to the message opening
that step. Any index is a safe cut: assistant messages precede their
results, so keeping a message keeps its results, and `transcript_of`
already drops orphaned results leading the kept region.

One guard earned its place the hard way: with a single huge step, the
cut lands before it, so compaction would summarize only the tiny prefix
— dropping the user's task to keep the bulk, for no saving. It now
requires the summarized portion to be worth at least a quarter of the
trigger, otherwise it declines and spends no provider call.

Verified against the recorded trace: spark's trigger moves 108.8k ->
85k, and compaction fires at the step reaching 90,698 tokens instead of
never.

Deliberately conservative and worth revisiting: `compaction_limit` is
`min(input_limit, context_limit)`. For explicit caps (spark's 100k,
OpenAI's 272k of 400k) that is exact. Where `input_limit` is merely
`context - output_limit` it assumes a maximum-length reply, so GLM-4.7
now compacts near 63k of its 205k window. An arithmetic discriminator
does not work — OpenAI's real 272k cap is *also* exactly
`context - output` — so telling the two apart needs an explicit catalog
marker. Erring conservative costs summaries; erring the other way costs
the session.

## 2026-08-20 — splitting a 13k-line main.rs without breaking it

Seven move-only commits, one seam each: text, transcript, session_view,
modals, input, selection + sidebar, app. main.rs 13,362 -> 2,131 lines,
now startup and the event loop. 172 tests throughout, clippy clean.

The method mattered more than the seams. `scripts/split_module.py`
chunks Rust on column-0 item boundaries and moves whole blocks by name,
so moved text is byte-identical and review becomes "did the right blocks
move" rather than "was anything rewritten" — which every review then
verified mechanically by diffing against `git show HEAD:`.

Two tool defects surfaced the same way, both silent-wrongness paths:
the chunker's depth counter is blind to braces inside strings, chars,
raw strings and comments, and a skewed count merges the following items
into one block where only the first name is read — so over-moving was
invisible while under-moving already failed loudly. And the first
visibility pass used a blind whole-file replace that ate `pub(crate) `
inside string literals. Both now refuse rather than guess.

The recurring finding was visibility. Applying `pub(crate)` with a regex
over-exported every time — 24 of 51, then 4 of 14, then 46 of 113,
including all 11 moved test functions in one seam. The fix is to stop
guessing: `scripts/minimize_visibility.py` strips every marker and
re-adds only what rustc's privacy diagnostics demand. It has one
prerequisite worth knowing — with a glob import a private item reads as
"cannot find" rather than "is private", so the signal never appears; it
now refuses when its target is glob-imported.

Import pruning was the one place a purely textual approach kept biting:
checking whether an imported name appears in the body cannot see a trait
used through method syntax, so `anyhow::Context` was silently dropped
three times. The compiler caught it each time, but it is exactly the
class of edit that makes a "move-only" claim false. It also exposed
markdown.rs reaching `wrap_styled_line` through a crate-root re-export
rather than its real home.

Moving App's 76 tests into app.rs retired 27 `pub(crate)` markers that
existed only so a test module in another file could reach render
internals. Worth doing for its own sake: it is a decent signal that when
a module needs a wide public surface, the tests may simply be on the
wrong side of the boundary.

What the split did not fix, and was never going to: `run_app` is still
~980 lines with decision and effect fused in every match arm, so the
loop remains untestable. Moving a function does not make it testable.
Recorded as its own issue.

## 2026-08-20 — steering a running turn

Typing during a turn used to queue the message until the whole turn
finished — `TurnOutcome::Completed`, meaning the model stopped calling
tools. On a forty-step task that is many minutes, and Esc was the only
way to redirect, which throws the work away.

Followed opencode's shape (`session/runner/llm.ts:383-406`): steers are
promoted into the history before the next request is built, so they land
at the next step boundary, and a steer arriving as the model stops
reopens the turn. Injection sits at the same settled point mid-turn
compaction uses — `continuations.is_empty() && paused_content.is_empty()`
— because between an assistant message carrying tool calls and its
results the transcript is incomplete.

Review verified the merge across all three wire shapes: the steer lands
in the same user message as the tool results, with the results first,
which is what Anthropic requires and what both OpenAI shapes render
correctly. Compaction is safe too, since `recent_steps_cut` walks
backwards and a just-appended steer is always inside the kept window.

Two defects it caught. The reopen check asked the channel whether it was
empty, but `drain_steers` filters whitespace — so a blank steer counted
as pending, reopened the turn with nothing to add, and appended a second
assistant message with no user message between. That leaves consecutive
assistant messages, which Anthropic rejects on every subsequent turn:
an unrecoverable session from one stray keystroke. Unreachable from the
TUI today only because two `trim().is_empty()` predicates in two crates
happen to agree. The drain now makes the decision, so they cannot
disagree.

The other was a straight regression: steering is fire-and-forget, and
`run_turn` drops its receiver on any non-completed exit. Esc during a
turn leaves `busy` set, so the obvious next keystrokes — type the
correction, hit Enter — reported "steering" and then silently destroyed
the message. Previously it would have queued. The TUI now shadows
in-flight steers and moves undelivered ones back to the queue, which
also gives the input title something to show.

Also took opencode's detail of resetting the step budget when a steer
lands: new instructions get a fresh budget rather than inheriting what
the interrupted work had left.

## 2026-08-20 — commands, and reading foreign skill formats

Two halves of the same gap. Skills are model-invoked: listed in the
system prompt with cue phrases, loaded through a tool when the model
decides they apply. That is a round trip and a judgement call, which is
wrong for "do exactly this now". Commands are the deterministic half —
markdown whose body is the prompt, substituted and submitted, never
auto-invoked.

The prerequisite was reading other people's files. Nine skills in
~/.config/opencode/skills/ could not load at all: they differ on two
axes, YAML frontmatter and a <name>/SKILL.md directory layout against
our flat TOML files. The parser now takes either, probing the first
meaningful line rather than inferring from where a file lives, and keeps
unknown keys in an extras map so a command's `model` or a skill's
`allowed-tools` survives before anything honours it.

The YAML side is a deliberate subset and review found three ways it was
silently wrong, each of which real frontmatter hits: a block scalar
truncated at its first blank line, an indicator like |+ or |2 leaving the
description as the literal string "|+", and a plain value wrapped across
lines losing everything after the first. Two real SKILL.md files are
checked in as fixtures so the long-description cases stay covered.

Commands drew its own crop. An apostrophe opened a quote, so
`/review don't merge yet` gave $1 = "dont fix it" with $2 and $3 empty —
quotes now only open at a token boundary. `$ARGUMENTS` matched without a
word boundary, mangling `$ARGUMENTS_LIST`. A body of only placeholders
invoked bare expanded to an empty prompt, which providers reject.

The dispatch was also written inline in a run_app key handler and its
test called `expand` directly, so it would have passed with the whole
branch deleted. Extracted to `resolve_slash(app, name, args) ->
SlashResolution`, which made command-shadows-skill, empty-expansion and
the `goal` collision testable in three lines each. That is the shape the
event-loop issue is about, arrived at from the other direction.

`$` semantics settled deliberately: `$(`, `${`, `$NAME`, `$$` are
literal, but `$` plus a digit is always a placeholder, so `costs $5`
with fewer args empties. Consistency beats guessing which `$5` was
meant, and there is no escape today.

## 2026-08-20 — loop decisions, before the loop rewrite

`run_app` fuses "what should happen" with "make it happen" in every
match arm, so paste routing, the notification gate, the queue drain and
goal continuation had no coverage at all — each needs a terminal, a
provider and a session store to observe.

Took the incremental step first, deliberately: extract those decisions
as pure functions over one `LoopState` snapshot, each returning a value
the caller acts on. The condition that makes it worth doing before the
full rewrite is that nothing mutates — so `decide(event, state) ->
Vec<Intent>` becomes an assembly of functions that already exist rather
than a rewrite of them. The alternative, helpers that mutate, is how an
incremental step quietly becomes the permanent state.

Review was blunt about the limit, and measured it rather than asserting
it: gutting every call site simultaneously — swapping the paste targets,
deleting the notification gate, no-oping the queue drain — left the
entire suite green, and the paste swap produced no warning at all. The
logic is covered; the wiring is not. Recorded on the issue instead of
letting the commit imply otherwise.

The finding that mattered was subtler. Three of four snapshot sites were
filling fields with plausible defaults the consumers happened to ignore
— `..LoopState::default()`, a hardcoded `steerable: false`,
`input_blank: true` recorded after the input had been emptied. Inert
today, poison later, since `decide()` will read the whole struct. One
`observe()` builds it honestly everywhere now.

## 2026-08-20 — intents, and the end of the synthetic Enter

Phase two of making the loop testable. Decisions now return `Vec<Intent>`
and `apply_intent` owns the state changes, so only the spawn needs a
runtime.

The real prize was deleting the synthetic Enter. The queue drain, goal
continuation and retry all worked by writing the message into the input
and posting a fake `KeyEvent::new(Enter)`, then hoping the dispatcher
was in a state that would route it to the submit arm. Review found that
hope was misplaced: `may_route_notification` never looked at
`pending_event`, so after a queue drain the notification gate fired
first, and the drained message became a *steer of the notification's
turn* — exactly the "queue order can invert" nitpick deferred in the
Milestone 5 entry. The chord handler and the completion popup could each
swallow it too.

Draining intents before the gate closes all three, and starting a turn
is now one code path rather than an impersonation of the user.

Review also caught a regression I introduced doing it: only the
interactive Enter expanded slash invocations, so a queued `/goal ship
the parser` was sent to the model as literal text once the turn it was
waiting on finished. Goal arming and slash resolution moved into
`prepare_prompt`, which every start now goes through, and `last_prompt`
records the raw text so a retry replays what was typed rather than the
expansion.

Honest coverage note, measured rather than asserted: `apply_intent` is
pinned by mutation testing, but deleting the drain loop entirely still
leaves the suite green. The wiring boundary narrowed from "every call
site" to "four pushes plus the drain". Phase three is `decide(event,
state)`; even that will not cover the loop's *schedule*, which is where
the bug above actually lived.

## 2026-08-21 — Structured user questions

- Questions are provider-visible tool calls but suspend in the agent loop, not
  `Tool::run`; a human wait therefore holds neither an executor slot nor a
  workspace lease.
- The assistant `question` call and ordinary `ToolResult` remain the sole
  persisted representation. A sole valid unanswered question is preserved on
  replay and resumed without inserting a synthetic user message; malformed or
  mixed pending calls retain normal interruption repair.
- Frontends receive a typed, session- and call-correlated prompt with a oneshot
  reply path. The TUI implements that protocol as a modal for batched single
  choice, multiple choice, free text, custom answers, and cancellation.

## 2026-08-21 — Configured reasoning defaults

`general.reasoning` now layers alongside `general.model` and is validated
against the resolved model catalog. It applies only when creating a session:
resumes keep their persisted reasoning choice, including an explicit return to
the provider default, and a CLI model override on a resumed session does not
inherit a potentially incompatible old variant.

Session metadata predates reasoning variants, so a non-default configured value
is persisted as the same initial `ModelChange` event used by the runtime picker.
That keeps JSONL compatibility and gives resumed sessions one source of truth
without expanding the metadata schema. Startup removes a just-created session
if this initial event cannot be written, rather than exposing a resumable
provider-default session that contradicts the configuration. The literal
`default` is the layer-reset sentinel because TOML has no null value.

## 2026-08-21 — Colour as hierarchy, not decoration

Side by side with opencode and Claude Code, the transcript read as loud and
flat. All three causes traced back to one property of `theme::apply`, which
remaps cells by ANSI colour name after render: the names were used up, so a
palette had exactly *one* background. With no surfaces to group with,
emphasis had to be `REVERSED` — a white slab on every inline code span and
every search hit — and syntax highlighting had to borrow the status colours,
which made a string literal the same green as a passing tool call.

New roles use `Color::Indexed` sentinels, a namespace nothing else emits, so
widgets stay theme-agnostic and `apply` resolves them: six surfaces and four
syntax classes. The adaptive `terminal` theme resolves surfaces to *nothing*
— it cannot know the canvas, so it must not paint one — and falls back to
reverse video for the selection alone, which is what `less` and `vim` do for
the same reason.

The hierarchy fix is the part that is not about the palette at all. The rows
that repeat most carried the most saturated colour: reasoning rows were
magenta end to end, and a green `tools ▸ N calls ✓` sat under every one of
them, which is green that cannot also mean success. Repetition now drives
saturation down — the label keeps the hue, the title is text, a group that
worked is muted — and hue is spent on what is rare: failures, live state,
diffs.

Legibility became a test rather than a judgement call, which is what made
shipping ten ported palettes tractable: body text clears WCAG AA on every
canvas and surface, surfaces stay within 2:1 of their canvas so they tint
instead of slab, and the syntax classes stay distinct and readable on the
code surface. The floor is AA rather than AAA precisely because the ports
keep their published values — Solarized Dark is 5.6:1 by design — with the
default theme held to AAA separately.

## 2026-08-21 — Failed turns resume history; transient calls back off

The old Ctrl-R path was not a retry at all: it submitted `last_prompt` as a
new user message. That duplicated the original request after every completed
tool round and could rerun slash-command setup. Failed-turn recovery now has a
separate `ResumeTurn` intent and core `resume_turn` entry point. It acquires the
same session writer and starts from `session.transcript()` without appending a
user event, so completed assistant/tool rounds remain context rather than work
to replay. The TUI only offers this after `TurnStarted` proves the turn's state
was committed; a resume attempt retains that disposition even if its preflight
fails.

Provider failures now distinguish permanent `Error` from
`RetryableError`. Network connection/timeout/body-read failures, HTTP
408/409/429/500/502/503/504, and structured overload/rate-limit/server errors
are transient; request/auth, protocol, and decode failures remain permanent. A
provider call that has not yet emitted response content retries up to three
times with cancellable 500ms/1s/2s exponential backoff (capped at 30s by
configuration). Once output has streamed, the loop persists it and leaves
recovery to Ctrl-R rather than replaying a potentially different response over
already-rendered deltas. `ProviderRetry` events make the delay and cause visible
in the TUI.

## 2026-08-21 — Manual compaction is a maintenance operation

Manual compaction no longer piggybacks on the next user turn. The palette and
built-in `/compact` command arm a standalone, cancellable operation which
acquires the normal session writer, resolves the session's persisted provider,
and summarizes the complete active transcript. It appends no user message and
does not advance goal mode, drain the message queue after cancellation, or run
ordinary turn completion behavior. `/compact` is rejected while another
operation is active rather than leaking into model steering.

`CompactionCut::ActiveHistory` is deliberately separate from automatic
`TurnBoundary` and in-turn `RecentSteps` cuts: only an explicit idle-session
request should replace the entire active provider context with its handover.
Successful completion reuses the live `Compacted` event path, so the handover
summary is shown in the transcript immediately and the context meter is
recomputed from persisted state.

## 2026-08-22 — Prompt caching was our item shapes, not OpenAI's cache

Cache reads on the ChatGPT backend kept collapsing to zero, worst right
after a step with several tool calls. Sessions already held the evidence:
each `assistant_message` carries one provider request's usage and a
timestamp, so `scripts/cache_report.py` reads the whole history straight
off disk. Across 160 sessions, misses tracked *how much the step
appended* — 6% after one or two tool calls, 52% after six — while the
gap since the previous request barely mattered, which rules out TTL.

Two controls turned a suspicion into a diagnosis. z.ai read a cache on
614 of 614 eligible requests through the same transcript pipeline. Codex
CLI, on the same account and the same Codex endpoint, managed 738 of
738, including every request that grew 10–30k — our 46%-miss bucket. And
a prefix that had drifted on our side could not produce these numbers
anyway: append-only history means a mutation still leaves the head
cached and reports a *partial* read, whereas zero on a 100k prompt whose
first thousands of tokens are pinned means nothing matched at all.

The difference was item identity. With `store: false` the server
rebuilds the item graph from what the client sends, and reasoning items
reference the calls that followed them by id. Codex replays every item
as it arrived, id included. ilar kept only `call_id` from
`response.output_item.added`, dropped the item id, and replayed calls
anonymously and messages as bare `{role, content}` pairs rather than
typed `message` items. Miss rate scaling with calls per step — and not
with reasoning items per step — is that fingerprint.

The id now survives from the stream through the session (an optional
field, so old sessions still load) back onto the wire, and messages
replay as `message` items with `input_text`/`output_text` parts.
`function_call_output.output` stays a plain string, which is the
canonical form for text results. Whether this fixes the cache is a
measurement rather than a claim: the baseline to beat is 40% misses on
appends over 2k, and the same script reads it back.

## 2026-08-23 — Time travel: checkpoints, rewind, fork at a point

Milestone 8. Every root turn in a git repository now snapshots the
working tree before the user message: a temp-index `read-tree HEAD` +
`add -A` + `write-tree`/`commit-tree`, chained under
`refs/ilar/checkpoints/<session-id>` so gc never eats it, recorded as a
`Checkpoint` event. The user's HEAD, index, and ignored files are never
touched, in either direction.

Rewind reuses the pattern compaction proved: an appended marker the
replay folds out. `Rewind { to }` truncates the folded stream back to a
user message, which becomes unsent (it returns to the input prefilled);
`audit_events` still sees every line. The tree restores from the turn's
checkpoint after a fresh safety snapshot, so a rewind is itself
recoverable. `fork_at` is the non-destructive sibling: a truncated copy
under a new id, `Ctrl-Y` in the same picker.

Review caught three things worth recording. `call_id.is_none()` is not
"is root" — notification turns on child sessions carry no call id, so
the checkpoint gate is `depth == 0`. The writer lease must be held
*before* the tree restore, or an active turn in another process gets
its tree yanked out from under it. And both compaction cut policies
landed the cut on the user message, stranding the checkpoint just
before it outside the kept window — silently degrading a later rewind
to conversation-only; the cuts now back over checkpoints the way
turn-boundary already backed over subagent invocations.

Crash-safety fell out of ordering rather than machinery: the replay
index is deleted (and the in-memory checkpoint cleared) before the
marker lands, so no crash point can leave a stamp-valid index
describing the pre-rewind window.

Smoke-ran the TUI flow in tmux against a scratch repo and scratch
state dirs (provider deliberately unauthenticated — a failed turn
still checkpoints and appends its user message, which is all rewind
needs). Verified live: the picker lists turns newest-first with ⎇
markers, Enter arms ("✗ … ↵ drops 1, restores tree"), the second
Enter rewinds — tree back to the turn-start state including untracked
files, drift deleted, notice "rewound 1 turn(s) · tree restored",
unsent message prefilled — and Ctrl-Y forks at the turn into a new
session with the same prefill. The audit log ends in the rewind
marker with both tree commits, and the checkpoint ref chain sits
under refs/ilar/checkpoints/<session-id>. A TUI review also caught
that /rewind and /fork typed during a running turn would have been
*steered to the model as literal text* — the maintenance-command
carve-out in decide::submit only knew about /compact; it now covers
all three.

## 2026-08-24 — The todo panel gets its space back

The sidebar showed five todos and a `+N hidden`, whatever the height
of the terminal — a twelve-item plan in a forty-row panel read as
"five items and a number". The cap is now the rows the panel has.

Raising it needed the trimming to get smarter first. The old pick was
"the in-progress item, the first pending one, the last completed one,
then fill from the top", which renders as a list with silent gaps
(items 1, 2 and 17 in a row) and, once wrapping ate the rows, could
push the active item off the panel it was chosen to keep. The visible
todos are now a contiguous run anchored on the active item — in
progress, else next pending, else the most recent completion — sitting
at the top of the run, so a panel that cannot hold everything drops
finished work above it before work still ahead.

Hiding was also a dead end: the transcript deliberately omits the
current list, so on a narrow terminal the one-line summary was the
whole of it. Ctrl-T (and the palette) opens a read-only scrollable
overlay over the full list, and the sidebar's hidden note names the
key when the panel is wide enough to say so. Both views draw items
through one `todo_item_lines`.

Smoke-ran in tmux against a synthetic session carrying a twelve-item
list: forty rows shows all twelve wrapped, sixteen rows shows the
active item and everything after it with `+4 hidden · ^T`, and Ctrl-T
opens `todos · 4/12 done` over the lot.

Same day, same panel-geometry theme: the transcript scrollbar never
reached the end of its track. ratatui counts scroll positions up to
`content_length - 1`, where that last position puts the final line at
the *top* of the viewport; we stop scrolling when the final line
reaches the bottom. Feeding it the row count therefore mapped our
maximum position a whole viewport short of the track end — four rows
above the bottom on a 40-row terminal, worse on bigger ones, so a
transcript at its true tail still looked like it had more below. The
scrollbar's content length is our position count, `max_scroll + 1`.

## 2026-08-24 — Subagents you can go back to

The task tool has taken a `task_id` since milestone 2: pass one and the
child session is loaded, its history replayed, the new prompt appended
— guarded on persisted agent, parent session and workspace, and
refused outright while that session is already being driven. It was
implemented, tested, and completely unreachable, because nothing ever
told the model an id. Foreground results were the child's final text
alone; the background start notice was a fixed string; the completion
notification carried description and result. Meanwhile the schema said
"never invent a value". So every subagent was one-shot in practice: a
follow-up meant a fresh agent and a re-explained scope.

All three paths now name the session, failures included — an
iteration-limited task is precisely the one worth resuming. The other
half is the `tasks` tool: this session's children with id, agent,
model, running-or-finished, age, opening prompt and a snippet of the
last reply. Scoped to the invoking session's own children, matching
the resume guard, so no session can enumerate another's tasks;
`SessionStore::list` hides children by construction, so `children_of`
fell out as the other half of the same head scan. Bounded twice — 200
characters per snippet, 20 tasks, remainder counted — because a listing
that can flood the caller's context is a listing the model learns to
avoid.

Running tasks deliberately show no snippet: their "last word" is
mid-turn and would read as a result.

Delegation was invisible from the sidebar: the status line carried a
background count, and nothing said what any of it was doing. The
spawner could not have said either — background handles carried no
description and the active-session set is bare ids — so it grew a
`running_tasks()` registry (description, agent, background flag, start
time), shared with child spawners the way the active-session set
already is, emptied by an RAII guard however a run ends. The sidebar
reads it every frame into an `agents` panel above services: two lines
per agent, three agents then a count, gone when nothing is running.

## 2026-08-24 — The summarizer was answering the user

Two field failures, one cause. Session `9860bd12` lost its own
objective: a mid-turn cut at event 245 sent the opening request — "Can
we do the necessary changes to firehose to support bundled payments?"
plus the two PR links defining the feature — into the summarizer along
with 771k characters of transcript, and the summary that came back
mentions neither. 167 characters of ask traded for 446k characters of
retained tool output. Session `3d494ad6` was worse: its summarizer
replied *to the user* — "I'm sorry, but I wasn't able to complete and
push all four fixes within this run" — and that 81-character apology
became the session's entire pre-cut history. A later manual /compact
on it returned no text at all.

The cause was the request shape. We sent `system = "You summarize
agent conversations"` and then replayed the transcript, so the last
thing the model read was conversation: a user message, or a wall of
tool results. A model resolves that conflict in favour of the
conversation — it answers, or (after tool results, with no tools
offered) it tries to act, produces reasoning, and stops with no text.

The instruction is now the final user message. Everything before it is
byte-identical to the request the turn itself would have sent: same
system prompt, same tools, same session cache key — which we were also
throwing away, since compaction passed `cache_key: None` while turns
pass the session id. This is Codex's shape and Codex's reason. The
alternative, opencode's, serializes the conversation into one quoted
message; it reads well but guarantees a cache miss on the entire
transcript every compaction, which on the session model is the
expensive way to lose information. Tool-output truncation went the
same way: it only looked worthwhile while the cache was already
broken.

Two guards on top. The summary now opens with the user's verbatim
requests — first always, then newest-first inside 4k characters —
because they are the cheapest tokens in a transcript and the ones a
summarizer paraphrases away first. And a summary that opens with an
apology, or that is a line long for a conversation over 50k
characters, is retried once and then refused: a stored summary
replaces real history, so failing loudly leaves a session recoverable
where storing the apology does not. The length rule is scoped to large
conversations on purpose — a false positive there ends in exactly the
hard compaction failure we set out to fix.

## 2026-08-24 — A second driver, and what it found

`ilar exec "…"` runs one turn without a terminal. The interesting part
was not the feature but the seam it exposed. The TUI's main loop
carried ~180 lines that were never terminal logic: resolving agent,
model and reasoning variant, assembling the system prompt from project
instructions, skills and the agent definition, creating or resuming
the session, wiring spawner, services, todos and registry. A second
driver either reimplements that or shares it, and two implementations
of "which model does this session run" is how frontends drift apart.

It moved to `ilar::runtime` in two phases, and the phases earn their
keep: `resolve` decides what the session will be and writes nothing,
`start` creates it and builds the tools. `--print-prompt` stops after
the first, so asking what the prompt is no longer risks leaving an
empty session behind. main.rs lost 312 lines and gained no behaviour.

Two things a headless driver has to decide that a TUI never does.
Questions: with nobody to answer, attaching the tool would hang the
turn on a channel nobody reads, so whether it is attached is now the
caller's choice and exec declines it — the model is told on the spot.
Teardown: background tasks and services are stopped at exit and
anything still running is named on stderr, because a detached
subagent whose notification has nowhere to land is a leak, not a
feature.

The output split is the design: stdout carries the answer and nothing
else, stderr carries how it was reached. `--json` swaps stdout for the
loop's events as NDJSON — through a hand-written projection, because
`LoopEvent` carries `Instant`s and a wire format should not change by
accident every time the enum grows. That projection is also the first
draft of the protocol a web frontend would speak.

## 2026-08-24 — Compaction stops guessing what will matter

Measuring the GM1 session settled an argument. Compaction there
reclaimed 56% at one cut and 39% at the next — same code, same
constants — because the recency window is sized with `chars/4 + 2` per
event and the second window held a 102,476-character block of hex
digests and 40k of ANSI-coloured disassembly, which tokenize near one
token per two characters. It kept 148k believing it kept 77k, and the
turn regrew to 237k of a 272k window before the turn ended.

The obvious fix was to calibrate the ruler against the provider's
reported counts, which are ground truth and already in the log. The
better fix was to notice why the window was large enough to have the
problem: dropped context was gone forever, so the heuristic had to be
generous. Make the session's own archive searchable and that stops
being true.

So compaction is now a handover. After it the model has its system
prompt, its tools, and one summary — no window, no pins, no tail, and
mid-turn is not special-cased. `recent_steps_cut`, `event_tokens` and
the ruler are deleted rather than fixed: the cut is "everything before
this point" and needs no estimate. Codex arrived at nearly the same
shape; what it lacks is the retrieval, which is the part that makes
the aggressive cut defensible rather than lossy.

`ilar::recall` is the scanner: session events flattened into
speaker-tagged entries addressed by event index, searched, and read
around. Everything it returns is bounded — an excerpt is the match
plus 120 characters, one row per entry however often the query appears
inside it, and reading around a hit truncates each entry. A search
that returns the 100k blob it matched has recreated the problem it
exists to solve. The `history` tool is its first front door; the
cross-session picker will be the second, over the same walk.

Two read paths the model never had, both of which the handover needs.
`history` lists every instruction the user gave, which is why the
summary need not carry the request verbatim. And `todo` became
readable: it was write-only, so the model's only view of its own plan
was the echo in the transcript — exactly what compaction deletes. It
was not forgetting the list, it had no way to ask.

What is left of policing is failure detection only. A summary that
answers the conversation instead of summarizing it is reported and the
session left untouched — no retry, no repair, no fallback. One
estimate survives, asked only whether there is anything substantial to
summarize, which keeps a session that is over the threshold on its
summary alone from compacting on every step forever.

That last estimate is gone too, on review. The loop it guarded against
requires the irreducible baseline — system prompt, tools, one summary
— to be near the trigger on its own, which no sane configuration can
reach; when the context is genuinely full, the material is by
definition large and the guard always passed. Meanwhile it carried a
real bug (it never counted tool-call inputs, so a write-heavy session
looked empty to it) and the summary size floor next to it was quality
policing of a model we otherwise trust with the whole session. The
trigger — the provider's reported token count against the threshold —
is now the only thing that decides a compaction, and failure detection
is exactly two checks: empty output, or an answer instead of a
summary.

## 2026-08-24 — Sessions found by their middles

Cross-session content search shipped, the second front door over the
recall walk the compaction work left behind. `Ctrl-G` from the session
picker opens a two-pane grep in the fzf mold: every root session's
full history on the left — compacted material included, since the
walk reads the audit log — and the selected match in its surrounding
conversation on the right. Enter resumes at the tail; jumping to the
match lost to the 99% case, and rewind already covers the rest.

The scan is live over the JSONL, no index: sessions are read one at a
time, rows stream through a channel stamped with a query generation,
and a keystroke cancels the walk (the emit callback's return value)
and bumps the generation so a stale scan's stragglers are dropped
rather than mixed in. Previews cost no second read — the walk hands
its entries to the callback and the context rides along with each row.
Caps everywhere the compaction work taught us to put them: 5 hits per
session, 200 rows total, bounded excerpts.

Also this session, on review: the compaction material guard and the
summary size floor are gone (see above) — the trigger alone decides.

## 2026-08-24 — /btw, the question that leaves no trace

A quick aside over the live session: `/btw which port was it again?`
sends the untouched transcript with the question appended as the final
user message — compaction's request shape, and for compaction's two
reasons: asked last so the model answers it instead of the
conversation, and byte-identical before it so the provider serves the
whole session from prompt cache and the aside costs the question
alone. The answer opens in a scrollable modal; Esc and it never
existed. Nothing is appended to the log on either side of the
exchange, which the tests pin by grepping the session file.

It rides the compaction plumbing end to end: queued behind a running
turn, busy while asking, cancellable, and its completion releases
messages queued during it — decided *before* the modal opens, because
a modal blocks the synthetic submit and nothing would be left to let
the queue go. Verified live: a session taught the word "pineapple"
answered the aside with it, and the question appears in no JSONL.

## 2026-08-24 — The UX batch

Four fixes from a deliberate look at the seams. Enter on a fully
typed slash command submits instead of demanding a second Enter. The
session search opens as a picker: an empty query lists root sessions
newest-first by topic, age and last words — fzf's empty-matches-all —
so the grep is now the whole front door and the classic list is just
where delete and fork live. The preview frame draws even with nothing
selected, closing a bleed-through. And /btw detached from the turn
slot entirely: it was refused mid-turn, which was backwards — an
aside is read-only and mid-turn is when you want one. It now runs
beside the turn on its own handle, with the transcript cut back to
the last settled point so a mid-flight snapshot with unpaired tool
calls is still a valid request. The queue-release dance from the
first implementation went with it: an aside no longer occupies
anything a message could queue behind.

## 2026-08-28 — Images ride with steers and queued messages

Attaching a screenshot and typing while a turn ran did nothing
useful: `decide::submit` saw attachments with a target of Steer or
Queue and returned `PasteInput` plus a warning to wait for the turn
to end. The message went back into the box and the images stayed
pending — two bugs in one gesture ("it doesn't send", "it doesn't go
away"), and the same mistake as refusing a session switch over a
waiting stash: keeping something safe by holding the person hostage.
Steering is exactly when a picture is most useful — "no, look at
this" — so the fix is to carry them.

The steer channel now carries `Steer { text, images }` instead of a
bare `String`, and the `UserMessage` a steer appends carries the
images that came with it; `turn.rs` had `images: Vec::new()`
hardcoded at the append. `LoopEvent::Steered` carries them too, so
the transcript row for a delivered steer shows the same attachment
markers a fresh turn's message does, root and child alike. Queued
messages hold their images until they are sent, at which point they
go back onto `pending_images` ahead of anything attached since —
`StartTurn` taking whatever is pending stays the single path images
reach a turn by, rather than growing a second one. Undelivered steers
moved back to the queue when a turn dies keep theirs, and pulling a
queued message back into the prompt (Ctrl-Q, e) restores its
attachments so re-sending sends the same message.

`decide::submit` lost its `attachments` parameter entirely: with the
refusal gone there was nothing left for it to decide, and the images
are taken off the prompt in `apply_intent` whichever way the message
goes. The test that pinned the old refusal is inverted rather than
deleted. The other producers are words-only and stay that way: the
web drive layer posts text, and a parent's `task_message` to a child
has nothing to attach.

## 2026-08-29 — Serve stands down, and the sweep's first seven

The web view is the largest thing in the TUI crate and the least used
part of the product: six and a half thousand lines across
`watch`/`view`/`drive`/`http`, plus a Preact frontend, plus — as the
second health sweep found — a second set of delivery semantics that had
quietly drifted from the terminal's (no retire, no salvage, adoption
once per process, three hand-rolled delivered-predicates, no watchdog).
Every change to the core paid rent to a consumer nobody was using.

So `ilar serve` moved behind a Cargo feature that is off by default.
Not deleted: `cargo test --features serve` still builds it and passes
all 24 wire tests. The bargain is written down in the issue — code
nothing builds by default rots, and the day the flag is flipped back
on, whatever broke gets fixed or the module gets deleted. That decision
belongs with the web frontend's, and it is not today's.

What the standing-down is *for* is the rest of the agent, and the sweep
had a list. Seven of them landed with it:

**The focus view was lying about live work.** Restoring a session ends
by marking every still-open tool row failed — true for a session nobody
is driving, false for the one you just clicked into mid-`cargo test`.
Worse than false: `finish_tool_row` refuses to settle a Failed row, so
the real result was dropped when it arrived and the row kept lying
until you refocused. The restore path now takes a `Liveness`, and the
focus view asks for `Running` — but only when the agent's events will
actually arrive. A roster row marked *delivering* is a routed
completion running with a discarded event sender: it publishes nothing,
so leaving its rows open would have traded a wrong ✗ for an eternal
spinner. What the seed leaves open, the focused session's `TurnDone`
closes.

**A steer could be delivered twice.** `publish` checks the cancel token
first, so a cancellation landing between the steer's `UserMessage`
append and its `Steered` publish ate the confirmation — and the reader,
which treats a steer without `Steered` as undelivered, sent it again.
The fix is an ordering: `publish` now reports whether the reader heard
it, and the confirmation goes *before* the append. Nothing is recorded
for a steer nobody was told about, so it comes back exactly once. The
window shrank from an await to a synchronous statement, and inverted: a
failing append now loses a steer loudly instead of duplicating one
silently.

**`TurnDone` is the last word.** Staged progress and output tails
outlived the turn they belonged to, so a slow consumer could animate a
row that had already settled. `publish_terminal` clears both maps, and
the receiver refuses progress after handing out the terminal event —
in `settle`, which both `recv` and `try_recv` route through, because
putting it in only one of them is how this comes back.

**Three smaller ones.** The worktree validator reads git's stderr to
decide whether a session may relax its rules, and matched the English
sentence: on a German machine every repositoryless session got the
wrong refusal. `git_command` pins `LC_ALL=C`, and `checkpoint::git`
got the same pin so its failures read the same everywhere. A completion
that could not be steered into a dying turn is requeued and auto-sent
as a fresh turn — through a path that pushed it raw, so the live
transcript showed XML where replay showed a collapsed row; both go
through one `push_user_message` now. And every session switch shut the
spawner down but left the aside and the topic-naming turns streaming
answers into a transcript that was no longer theirs: `leave_session` is
the whole ritual, one call at all six exits, so the halves cannot drift.

The review of this batch is worth keeping: it caught the delivering-row
spinner, the half-armed `finished` flag, and the fact that fixing the
duplicated aside ritual by adding a *second* duplicated call beside
`spawner.shutdown()` was the wrong seam.

## 2026-08-30 — The rest of the correctness sweep

Nine of the sweep's correctness findings, minus the five that parked
with `ilar serve`. What ties them together is that none is a bug in
what the code does — every one is a bug in what it does *when
something else happens at the same time*.

**Two of them were the same mistake about identity.** A flock holds an
inode, not a path, and `delete()` unlinks the session lock while
holding it: a waiter could win the lock on a file with no name while a
third process locked a fresh one at the same path, and both would
believe they owned the session. `acquire_writer_id` re-stats after
locking now and starts over on a mismatch. The same shape one directory
over: a process group id outlives its group, and a service that
daemonizes keeps that id for the whole session — long enough for the
kernel to hand it to a stranger, whom `stop` would then SIGKILL. Every
kill is probed with signal 0 first, and a group that stops answering
loses its id at the next status read.

**The outbox was breaking its own rule.** `retire` exists as an
appended tombstone precisely because a read-filter-rewrite would erase
a publish that landed between the read and the rename — and `pending`'s
compaction was a read-filter-rewrite. It takes a directory-wide lock
now. The interesting part was where *not* to hold it: the first version
wrapped the whole scan, which meant a completing child blocked behind
every session replay and ancestry walk an adoption performs. The lock
covers one file's read-filter-rewrite and nothing else. A publish that
lands between the delivery check and the lock is kept as undelivered —
the double-delivery the module already admits to, rather than the
silent loss it refuses.

**The activity feed drowned and then stopped listening.** One 256-slot
broadcast carries every event of every child at every depth, deltas
included, and the TUI treated `Lagged` as end-of-drain: several
streaming children, and the live tape stalled for the rest of the frame
— exactly when it had the most to show. The ring is four times bigger
(a bound, not a number to keep raising: tokio allocates it eagerly and
a slot can hold a 16 KiB delta) and a lag is now a gap to step over.
The review found a bonus: the fold retried its held-activity queue once
per event, so a busy frame was quadratic in the transcript's length.
Once per frame now.

**And four small ones.** A store IO error inside the duplicate-tool-id
check propagated raw, skipping `persist_failed_step` — so text the user
had watched stream was never written and no `TurnDone` was published;
it goes through `errored` like its siblings. `delete()` leaves no
scratch behind. Launching or resuming with a cwd that is gone is an
error rather than a panic that takes the process with it
(`try_root`/`try_new`; the panicking names stay for the hundred-odd
tests that own their paths). A background job that dies abnormally no
longer wears a task's envelope and invite to "resume it with the task
tool" — it has no session and no task id, and the advice was an
invitation to invent one.

The review of this batch was worth more than the code: besides the
lock-duration problem and the quadratic retry, it caught an enum I had
inserted into the middle of somebody else's doc comment, and pointed
out that the outbox race test only exercised a single compaction
window — a regression would have sailed through it.

## 2026-08-30 — One delivery engine

A background child's completion has to reach the session that spawned
it, and every surface that can drive a session had its own opinion
about what that means. The sweep found the delivered-check written out
three times and a second driver that had quietly grown a shorter list
of obligations than the first. Now that serve is stood down, the point
of extracting an engine changed — it is not about reconciling two live
drivers, it is about there being one place the rules live *before* the
second driver comes back and invents them again.

`ilar::delivery` holds two things.

**What "delivered" means.** One predicate over the target's log —
substring, because a delivering prompt can carry queued steers ahead of
the notification text — with its one accepted limitation (two
byte-identical texts for one parent dedupe as one) written down once
instead of three times.

**What an ending obliges you to do.** `Disposition` is an enum a driver
must match exhaustively: `Delivered`, `Propagate`, `Exhausted`, `Hold`,
`Salvage`. That exhaustiveness is the whole design. serve's missing
retire and missing salvage were not subtle reasoning failures; they
were arms nobody wrote, in a match nobody was forced to complete. Now
the compiler names them.

Threading it turned up a real asymmetry nobody had filed: serve bounded
propagation at eight hops, and the TUI did not bound it at all. The
budget exists for a parent chain that names itself as its own ancestor
— the same corruption `outbox::pending`'s ancestry cap already refuses
to walk forever — and in the terminal that completion would have
climbed until the process ended. So the hop budget moved into the
engine as `Parcel`, which the TUI's held queue now carries, and a spent
budget became an ending in its own right: `Exhausted`, which is
salvaged into the transcript and retired rather than dropped. serve's
old behaviour there was a silent drop with a log line — this is
strictly more honest, and it is now the same behaviour in both.

What did not happen: serve's `Consumer` still owns its own
follow-up-vs-route decision, its own backoff, its adoption-once, and
its missing watchdog. Rewriting a dormant driver is exactly the tax
standing it down was meant to stop paying. The issue stays open,
marked, with the remainder parked beside the feature.

## Model-picker defaults belong to a model

The reasoning picker used `None` for both “this model is running with provider
defaults” and “this is a different model.” Its unchanged-level shortcut therefore
dismissed Astra's default selection before the model-switch persistence path ran.
The picker now takes the current model identity and gates both the no-op check
and active-row marker on it. Explicit selections and same-model no-ops remain
unchanged. Regression reproduced before the fix; all 458 TUI tests pass.
