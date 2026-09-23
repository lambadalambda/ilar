# The interface

Press **F1** for the full keybinding reference — any time except
under a grant prompt or a question, which outrank it and take the key
themselves. This page covers the parts that deserve more than a
one-line hint.

## Keys your terminal has to be able to send

Some chords do not survive an ordinary terminal. A bare terminal sends
one byte, a carriage return, for **Enter**, **Shift-Enter** and
**Ctrl-M** alike, so pressing any of them sends the draft. Telling them
apart takes an extended-keys protocol — kitty's, or xterm's
modifyOtherKeys in its `csi-u` form. ilar offers only the chords it
will actually receive: the footer and F1 name **Shift-Enter/Ctrl-J**
where the two can be told apart and **Ctrl-J** alone where they cannot.
Ctrl-J is the literal line feed and always works, so a draft can always
gain a line.

tmux is the usual reason a capable terminal looks incapable: it ships
with `extended-keys off` and will not pass modified keys through. If
Shift-Enter sends instead of inserting a newline, put this in
`~/.tmux.conf` — these are server options, so `-s`:

```
set -s extended-keys always
set -s extended-keys-format csi-u
set -as terminal-features '*:extkeys'
```

`extended-keys` needs tmux 3.2 and `extended-keys-format` needs 3.4; on
anything older, leave the second line out and expect the first to do
less.

Three things about that recipe are load-bearing.

`always` rather than `on`, because `on` forwards extended keys only
once the application has asked, and tmux answers nothing to the query
that does the asking. Under `always` it sends them regardless.

`csi-u` rather than the default `xterm`, because the two formats are
not interchangeable: crossterm, which reads ilar's input, parses
`CSI 13;2u` and has no handler at all for xterm's `CSI 27;2;13~`. With
the wrong format Shift-Enter is not misread, it is dropped.

`terminal-features` with `extkeys`, because tmux asks the terminal
outside it for extended keys only when it believes that terminal can
supply them.

One consequence worth knowing. Because tmux never answers the startup
query, ilar begins by offering Ctrl-J alone even where Shift-Enter now
works. The first Shift-Enter you press settles it: a modified Enter
could not have arrived as a bare carriage return, so ilar takes that as
proof and names both from then on. It is proof about Shift-Enter only —
under `always` tmux still sends a bare carriage return for Ctrl-M, so
that chord stays hidden and **F2** remains the way to switch models.

## Starting

A bare `ilar` in a directory you have worked in before offers that
directory's last session, shown rather than described: its tail is
drawn in the transcript pane in a muted, ghostly style under one header
line —

```
previous session here: fix the flaky adoption test · 2h ago
```

The empty prompt below says how to answer it, muted:
`Enter resumes · type to start fresh`.

- **Enter** on an empty prompt resumes it, exactly as the picker's
  resume does.
- **Typing** leaves it behind: the ghost clears on the first character,
  and what you write goes to the fresh session you are already in.
- **Esc** on an empty prompt dismisses it without starting anything.

Anything else — scrolling, F1, the palette — leaves the offer up. The
ghost is a bounded read from the end of the log (a couple of screenfuls,
whatever the session weighs), it is never written anywhere, and the
status line says `ghost of <session>` while it is on screen. The fresh
session you started in is removed on quit if you never said anything in
it, so dismissing an offer and leaving costs nothing.

The offer is only made when the launch named no session: `--session`,
`--continue` and `--view` have all already said which conversation this
is. `general.resume_offer = false` turns it off — see
[configuration](configuration.md).

## The status line

During a turn the status line reads like:

```
○ thinking · 84.2 KiB · 12.3 KiB/s   zai/glm-5.3   in 300 · out ~8400 · cache 86% · Σ 1m $0.42 · ctx [██░░░░░░] 24%
```

- **Activity + liveness** — `thinking · 84.2 KiB · 12.3 KiB/s`: bytes
  streamed this turn and the current transfer rate. A silent stream shows
  `· no data Ns` after 3 seconds; `0 B · no data Ns` means the provider
  has not sent a single byte. The spinner alone proves nothing — only
  these numbers do.
- **`in` / `out`** — the last provider request's token usage. While a
  step streams, `out ~N` is a live estimate from streamed bytes
  (~4 bytes/token) and snaps to the exact reported value when the step
  completes.
- **`cache N%`** — how much of the last request's prompt the provider
  served from its cache, billed at the cheap cache-read rate. A healthy
  agentic session sits high and climbs as the conversation grows; a drop
  to 0% means the cached prefix was not matched (model switch, prompt
  change, or provider eviction) and that request's cost and latency just
  went up. `cache —` means the request had no prompt to speak of. The
  palette's "Session usage" entry still has the raw read/write counts.
- **`Σ tokens $cost`** — session-cumulative totals across all turns,
  priced per-step at each model's list rates (cache reads at the cache
  rate). Coding-plan models show `plan` instead of dollars; unknown
  models show tokens only. The palette's "Session usage" entry has the
  full breakdown.
- **`ctx …%`** — estimated context usage against the model's window
  (`~` marks estimates); compaction triggers at `compaction.threshold`.
  `/context` (or the palette's "Set context window") overrides that
  window for this session only — `/context 128k`, `/context 200000`,
  `/context default`, or bare `/context` for a picker — and drives both
  the `ctx` percentage and when compaction fires; for discovered or local
  models that report the wrong window.

## Steering and the queue

Type while a turn is running and the message **steers** it: the loop
delivers it at the next step boundary rather than after the whole task,
and a steer arriving as the model stops reopens the turn instead of
stranding the message. Until delivery, every pending message is listed
in a strip above the input with its fate — `steering · next step` or
`queued · when the turn ends` — and its row disappears the moment the
model actually receives it, which is also when the text appears in the
transcript. A subagent's result waiting its turn is listed the same way
as `task result · next step`, by the one-line headline its transcript
row will wear, so mail never reads as something you typed. If a turn ends without delivering a steer — you aborted, or
it errored — the undelivered steers move to the queue rather than
vanishing. Turns with no steer channel (a notification routed from
another session) still queue as before.

A `/command` is not steering text. Submitting one into a turn that can
take a steer is refused — `wait for the current operation before /goal`
— with the text left on the prompt, because the commands are armed,
expanded and routed on the way *into* a turn: steered, `/goal ship the
parser` would reach the model as that literal line and arm nothing.
`/btw` and `/context` are the exceptions, and are meant for mid-turn.
A command with nowhere to steer (a notification turn has no steer
channel) queues instead and runs when the turn ends, since the queue
drains through the same path a typed turn does — except the
maintenance commands (`/compact`, `/rewind`, `/fork`, `/sessions`),
which refuse whenever anything is running.

Standing state — queued messages, the goal, background tasks, held task
results, a retry offer — is managed in the pending manager (**Ctrl-Q**
or the palette): delete one queued message, pull it back into the input
for editing, abort the goal or cancel background tasks (both confirmed
with a second press). Enter on the tasks or services row acts on it —
the same confirmed cancel `d` gives — since there is nothing there to
edit; Enter on a held task result delivers it without spending a turn
on the asking.
**Esc is strictly immediate-scope**: it aborts the running turn or
clears the input, and never touches the queue or the goal. A one-line
draft it clears; a multi-line one — a paste, or a paragraph — goes to
the stash instead, because Esc has no undo.

One thing does follow the turn down: the detached tasks *that turn*
started, whose cancellation rides on the turn's own. Aborting therefore
pauses notification delivery the way cancel-all does — the dying
children's results are held, not delivered, so the abort does not
immediately start a follow-up turn nobody asked for. They go out with
your next message, and tasks from an earlier turn keep running
untouched.

The palette (**Ctrl-P**) opens during a turn as well: the pending
manager, help, the link picker, an export and the usage line all work
there. Switching model or reasoning does not, and says so rather than
doing nothing; the same goes for F2 and Ctrl-X mid-turn. A theme
change (**F3**) is only paint, so it is never refused.

## When the provider stumbles

A failure before any of the reply has arrived is retried in place: three
times with a short doubling wait for ordinary hiccups, six times with a
longer one for rate limits, honouring the server's own wait when it names
one. A failure *after* the model has started speaking cannot be replayed,
so the partial step is committed to the log as it stands — with a
diagnostic and an error result for any tool call it had announced — and
the turn continues from there with a fresh request, at most twice per
turn. The transcript shows the seam as a line reading `provider dropped
mid-step (…) — continuing (1/2)`. Past that budget the turn fails, the
error stands in the notice line, and **Ctrl-R** resumes from the same
committed state by hand.

A turn you abort yourself leaves the same committed chain behind, so the
same **Ctrl-R** continues it — that is what the stall watchdog's notice
means when it offers Esc. The offer survives the session, too: open a
session whose last turn died and the notice says so, with Ctrl-R still
armed. Ctrl-R with nothing to resume says so rather than doing nothing.

A different kind of stumble is the one you cause by walking away: once
the provider's prompt cache has dropped the session, the next move re-reads
everything at full price. With `cache_compact.enabled` set (see
[configuration](configuration.md#compacting-while-the-cache-is-warm)) an
idle session compacts itself just before that happens, and says so in the
transcript and the notice line.

Half-written something when a more urgent message comes to mind?
**Ctrl-S** stashes the prompt — the same key on a blank prompt pops the
newest stash back, cursor at the end, with whatever images were
attached to it. Stashes stack, the input title counts them
(`· 2 stashed`) so nothing is forgotten, and unlike the queue nothing
is ever sent on its own: a stash only comes back when you pop it.

Because a stash lives in the running app and nothing else, the things
that would throw it away say so first: Ctrl-D on a blank prompt
warns once before the second press quits. That warning leads with the
key and then names everything leaving would take: the running turn, the
background agents cancelled with it, the goal and its round, the
stashed prompts and unsent messages that die with the process, messages
to an agent still in flight, and the task results waiting in the outbox
— which come back at the next open, and say so.

**Ctrl-L** clears and repaints the whole screen, including while a
modal or picker is up — which is when outside damage is most likely
and dismissing to fix it least welcome. Tool commands run in their own
session with no controlling terminal — a `sudo` password prompt fails
fast with a readable error instead of drawing over the UI — but
anything that still writes to the terminal from outside leaves damage
the diff renderer won't touch; Ctrl-L is the eraser.

## Images

Copy a screenshot, press **Ctrl-V**, and the image attaches to your
next message — listed above the input until it sends, shown as a size
marker in the transcript, stored inline in the session so it survives
restore and fork. Text on the clipboard still pastes the normal way;
Ctrl-V only intercepts images.

**Dropping files** onto the terminal works too: a drop arrives as
pasted paths, and when every token is an existing image file they all
attach instead of landing in the input — one stray word and the paste
stays text. PNG, JPEG, WebP and GIF are accepted (sniffed from the
bytes, not the extension) and travel as themselves — only oversized
PNGs are re-encoded on the way in.

The catalog knows which models see: every OpenAI model does, on z.ai
only the V-series (available on the coding plan too), and on OpenCode
whichever rows models.dev marks multimodal (the GPT, Grok, Muse Spark,
Kimi and Qwen families among them). Attaching on anything else is refused with a notice
naming the model. A draft's attachments travel with it whenever it
goes: as a fresh turn, as a steer into the turn already running, or as
a queued message. What a running turn refuses is *attaching* — Ctrl-V
while it works says so and asks you to wait, so an image is never
added to a message halfway out the door. Esc discards attachments
along with the draft. Oversized images are downscaled to fit 2048 px on the longest
edge before anything is stored or sent — providers shrink to that
before tiling anyway, so a retina screenshot costs a fraction of the
bytes with nothing lost. Three backstops: 64 MiB of file, weighed
before the file is read at all; 64 megapixels of picture, weighed from
the header before any decoder is asked to believe it — a few kilobytes
of PNG can claim to be 40,000 by 40,000, and something has to say no
before the frame buffer is allocated; and 10 MiB on the attachment as
stored, after the shrink. A clipboard image is decoded by the system
clipboard itself before ilar sees a pixel, so that one allocation is
outside these bounds; what ilar can refuse, and does, is re-encoding
something past the pixel limit.
Switching a session with images to a text-only model replaces them
with a named `[image omitted]` gap rather than an error.

Because the image is stored once and replayed byte-identically, it
becomes part of the provider's cached prefix like any other content —
follow-up turns read it from the prompt cache instead of re-billing
it.

**The agent can open one itself.** `read` pointed at an image file in a
vision session returns the image alongside its one-line description
(kind, dimensions, size), downscaled through the same 2048 px pipeline
as a paste; the transcript shows a `[image: png · 12.3 KiB]` marker
where the payload would be, live and on restore alike. On a text-only
model the description arrives alone, so nothing errors — it just does
not see. This is what lets a *vision subagent* inspect a screenshot the
parent produced: write the file, spawn a task on a vision model, tell
it the path.

## Asides: `/btw`

`/btw which port was it again?` answers a quick question over the live
conversation without becoming part of it: the model sees the whole
session, the answer opens in a scrollable modal, and neither the
question nor the answer is written to the log — an aside must never
steer the ongoing work. It runs beside a live turn (mid-turn is when
you want one), costs almost nothing thanks to the provider's prompt
cache, and a newer `/btw` replaces a still-running one.

## Switching sessions: `/sessions`

![the session search: matches across four sessions for "timeout", the selected hit previewed in context](assets/sessions.svg)

`/sessions` (or the palette's "Switch session", or just Ctrl-P → Enter)
opens a two-pane grep over every session you have:

- **Empty query**: your sessions with the ones from the directory
  you are in first, newest-first within each group — topic and when it
  was last used ("2h ago" inside a day, "aug 12" beyond). From 96
  columns up, the tail of the selected conversation is previewed on
  the right; below that the list takes the whole modal.
  So the row you open on is where you left off *here*; sessions from
  elsewhere follow, marked with their directory (`· ~/repos/foo`), and
  sessions nothing was ever said in come last. The list is capped at a
  couple of hundred rows, but this directory's sessions are taken
  first, so its last session is shown however many newer ones other
  checkouts have — and it appears immediately, straight from the
  pointer file, before the rest of the listing has been read.
- **Type anything**: rows become content matches from *every* session's
  full history, compacted-away material included; the preview shows
  each match in its surrounding conversation. Find a session by an
  error string you half-remember from its middle.
- **Enter** resumes the selected session at its tail. **`^G`** switches
  to the classic list picker, which orders and stamps its rows the same
  way, and is where title filtering, delete (`^D` twice) and fork
  (`^Y`) live.

## Session topics and the window title

After a session's first completed turn, ilar names it in a few words —
that topic appears in the transcript's title bar, the session listing,
the search, and your terminal's window title (`GM1 firmware dig`, just
`ilar` until the session has a topic) via the standard OSC escape.
Sessions from before the feature name themselves after their next
completed turn.

## Goal mode

`/goal <description>` keeps ilar working until the goal is demonstrably
achieved: after every completed turn it auto-continues (in the same
session, so the prompt cache absorbs the cost) with an instruction to
verify progress using concrete evidence — running tests or a harness,
building one if none exists — and to keep working otherwise. The loop
ends when the model outputs an evidenced `GOAL_ACHIEVED:` line, when the
round cap (25) trips, or when you abort it explicitly (`/goal abort` or
the pending manager). `/goal` alone prefills the input for editing the
goal in place, keeping the round budget. Aborting a running turn pauses
the loop; it resumes after your next completed turn.

The goal lives in the running app and nowhere else, so leaving the
session it belongs to is refused while it stands — a rewind, a fork, the
picker's `^Y`, resuming another session — with `a goal is active — /goal
abort before leaving its context`. Ending it deliberately puts a line in
the transcript saying how many rounds it ran.

## The sidebar

On wide terminals the right column tracks session state: the todo list
the model maintains as it plans (**Ctrl-T** opens the full overlay),
running services — every one that runs, with exited ones collapsed
into a count that clicks open to show who died how — and, while subagents are in flight, an `agents` panel
with each task's description, agent, a `bg` marker for detached work,
and a live elapsed time. A result on its way to another session shows
there too, as a ✉ `delivering` row, and when it lands the transcript gets
one quiet line — `✉ "review the diff" delivered to explore · survey the
API` — naming the session by its agent and task rather than by id. A
result that has to climb to another tree says so on its way — `✉
"review the diff" passed on to build · land the fix` — and a result
that cannot be delivered, or is held because the session it belongs to
is open elsewhere, claims the notice line above the input. A background job — `bash` with `run_in_background` — sits in the
same panel while it runs, as a ⚙ row with its command and elapsed time,
so a long render never reads as a hang; it has no transcript to open.
The panel's title counts each kind for what it is — `agents (2) · 1 job
· 1 delivering` — since a job is not an agent.

## Talking to a focused agent

Click an agents-panel row and the child's transcript fills the screen.
The prompt is then that agent's: the input title reads `to explore ·
survey the API`, the root's own draft is put aside until you leave, and
Enter sends what you typed the way the model's own `task_message` does —
a running agent takes it at its next step, a finished one is resumed
with it as the prompt. The root's transcript shows the send as
`→ explore · survey the API: …` and, when it lands, one line for what
became of it: `… takes it at its next step`, `… gets the message at its
next resume`, or `… answered: <the first line of its reply>` — the reply
itself stays in that agent's own view, rendered. Neither is written to
the session log, so a restart shows no trace of them.

Three rows cannot be messaged, and Enter says so before anything is
sent: an agent working inside the turn you are in (the turn is waiting
for its result), another session's agent, and an agent started by
another agent. Their footer offers no Enter. Slash commands are not
offered here and are refused if typed — they belong to the session
behind the view. So do the root's other chords (F1, Ctrl-P, Ctrl-Q,
Ctrl-F, Ctrl-T, Ctrl-S, Ctrl-D, …): pressing one names it and says Esc
leaves the view first.

Arrow keys, PageUp/PageDown, Home and End scroll the view. **Ctrl-G**
cancels the agent you are looking at — a second press confirms, since a
cancel has no undo, and the cancelled task's result is held rather than
delivered. It is the only cancel that takes one agent; Ctrl-Q's takes
every background task and every in-flight delivery with it. Esc leaves
the view, and anything typed at the agent and not sent goes to the stash
rather than becoming the root's next message.

## Questions

A model that needs a decision from you calls the `question` tool, and
the turn stops on a modal: one question per screen, its prompt and an
optional description, then the options. Arrows (or Tab) move, Space
picks — one option for a single choice, any number for a multiple one —
and a question that allows it has an "Other…" row you simply type
into. Enter takes the screen and moves to the next question; the last
one hands every answer back at once. Free-text questions are a text
field with the same Enter, and an arrow with nowhere left to go inside
it steps between questions instead. **Shift-Tab** goes back a question
without validating the one you are on.

**Esc** cancels the whole modal. That is an answer too, not a failure:
the tool comes back `{"status":"cancelled"}` and the turn goes on with
what the model already knew, so cancelling is the right move when the
question is one it should decide itself.

Only the root agent may ask, and only where someone can answer: a
subagent, `ilar exec`, and a turn started from `ilar serve` are all
told nobody is there to answer and to decide for themselves. A question
left pending when the TUI exits comes back on the next resume of that
session.

## Secrets

A stored secret (`ilar secret set NAME`: the value is asked for hidden
at a terminal, or piped in) never sits in
the agent's environment. When `bash` or `service` names one, the turn
pauses on a prompt titled `bash wants NAME`: the secret's description,
then the command **verbatim** — that exact text is what runs with the
value in its environment, so read it before saying yes. Four answers:

- **Allow once** (`o`, the default): this command only.
- **Allow for this session** (`s`): this tool, until ilar exits — and
  for the root agent *and* every subagent it spawns, since the grants
  given this session are one shared set.
- **Always allow for `tool`** (`a`): written to the store as a standing
  grant; the prompt does not come back for that tool.
- **Deny** (`d` or Esc): the tool gets a refusal and the model learns
  the secret is unavailable.

The answer is noted in the transcript (`NAME allowed for bash (once)`,
`NAME denied for bash`). A subagent's tool asks through the same
prompt, titled `bash (reviewer subagent) wants NAME` — named, so a
prompt that appears while several children work says whose command it
is. Ctrl-C under the prompt is a deny; if the turn ends or is cancelled
underneath it, the prompt closes without an answer, which the tool
reads as a refusal, and the transcript says so (`grant prompt for NAME
withdrawn — the tool stopped waiting`).

The prompt is approval only, whoever asks. When the asker is the `sudo`
tool the password — if sudo turns out to want one at all — comes in a
second prompt after the yes, titled ` sudo password `: the command
again under "For:", a masked field with a cursor (a window on the tail
once the mask outgrows the row), paste accepted, Enter to send, Esc (or
Ctrl-C) to cancel, which fails the sudo call. Enter on an empty field
says "sudo needs a password on this system" and stays up; a password
sudo refuses brings the prompt back saying so, up to three times, and
the approval is not asked for again. A held or stored password, or a
system with passwordless sudo, means no password prompt at all — see
[secrets.md](secrets.md#sudo) for the order. What you type is held in
memory for the session and never written, and a turn that ends under
the prompt withdraws it (`sudo password prompt withdrawn — the tool
stopped waiting`).

Both prompts have their own section in the **F1** overlay — read
beforehand, since a prompt takes every key while it is up. Standing
grants are managed from the shell: `ilar secret list`,
`ilar secret grant NAME --tool bash`, `ilar secret revoke NAME`.

## Transcript

The transcript renders markdown with syntax-highlighted code fences and
diffs for the tools that change files — an `edit` as a real diff, a
`write` as the body it wrote, labelled `rewrite` when it replaced a
file that was already there. **Ctrl-F** searches it, **Ctrl-O** opens any link it
contains, mouse drag selects and copies, and the palette's "Export
transcript" writes the session as a Markdown file.

ilar holds the mouse for as long as it runs, which is what its own
selection needs and which takes the terminal's away. Hold **Shift**
while dragging to get the terminal's selection back; that one belongs
to the terminal and ilar never sees it. A selection ilar does make goes
to the clipboard on release, and over SSH it asks the terminal to do
the copying (OSC 52) rather than reaching for a clipboard on the host,
so the text lands at your end of the connection. The protocol has no
reply, and some terminals ship it turned off, so a copy that went
quiet was sent and may not have been honoured. Tool rows expand on
click (or Enter targeting) to show arguments, diffs and output — and a
truncated block's "… more" row is itself clickable, advancing the
expansion right where the eye stopped; grouped tool calls align their
columns to the widest sibling. A collapsed group still shows what is
running and what failed, and a failed row carries the error's first
line after its arguments. Anything
clickable underlines itself when the pointer hovers over it, and
clicks resolve against the row that was under the pointer when the
button went down — a streaming turn cannot pull the target out from
under a click.

## Themes

`general.theme` or the **F3** picker. Authored: `carbon`, `parchment`,
`frost`, `high-contrast`, `terminal` (adapts to your terminal's own
palette). Ported: `monokai`, `dracula`, `gruvbox-dark`,
`gruvbox-light`, `solarized-dark`, `solarized-light`, `tokyo-night`,
`catppuccin-mocha`, `one-dark`, `rose-pine`.
