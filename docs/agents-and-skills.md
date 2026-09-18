# Agents, skills, and commands

All three are markdown files. Agents define who does the work, skills
teach the model how to do something on demand, and commands are canned
prompts you invoke yourself.

## Custom agents

Custom agents are Markdown files in
`${ILAR_CONFIG_DIR:-~/.config/ilar}/agents/` and `./.ilar/agents/`. Project
definitions override user definitions with the same filename, and user
definitions override built-ins. Agent frontmatter supports `description`,
`model`, and `disabled`; for subagents it also supports `read_only` and `tools`
(an allowlist of tool names; unknown names are a load-time error, and the list
intersects with the read-only set when `read_only = true`). Tool restriction is
coordination, not a security boundary. A file with `disabled = true` is
skipped; it does not remove a lower-priority definition with the same name.

## Subagents and tasks

Built-in subagents are `build` (mutable, serialized per checkout), `explore`
(read-only, safe for parallel repository inspection and review) and `review`
(serialized like `build`, with a shell and without the write tools: it may run
tests, builds and git, and reports rather than fixes). `read_only` is
a toolset, not a promise of good behaviour: the agent gets `read`, `glob`,
`grep` and `webfetch` and nothing else — no shell, so no tests, no builds, no
git, no scripts. Delegate anything that must *run* something to `build`, which
is serialized per checkout precisely because running things collides. That is
a decision, not an accident of history: `read_only` means this list of four
tools, not "everything except what writes". A read-only agent takes a shared
read lease on the checkout, which is what lets several of them run at once;
hand them a shell and four parallel reviewers would run four `cargo test`s
over one target directory. Tools that read "read-only" as "no edits" (Claude
Code's reviewer runs `git diff`; Codex sandboxes effects, not commands) have
no lease to protect. A reviewer that must run things is a serialized agent by
construction: that is `review`, whose allowlist is `read`, `glob`, `grep`,
`webfetch`, `bash` and `secrets` — no `write`, no `edit`, no delegation, no
`sudo`. The allowlist is coordination, not a boundary (a shell can write);
the prompt says to report and not fix. It runs in the foreground like `build`,
since its findings are what the delegating agent waits on before committing,
and a detached serialized reviewer would hold the checkout against that
agent's own edits; `background: true` still detaches it. Tasks can
override the child's model per invocation (`model` and `reasoning` on the task
tool — e.g. a cheap flash model for mechanical sweeps); omitted, the child uses
the agent definition's model or inherits the parent's model and reasoning. The
read-only `models` tool lists available models with context windows, pricing,
and reasoning variants so agents can choose informedly.

A task's `background` follows its agent when the call omits it: a read-only
agent's task (`explore`) detaches and reports back as a completion
notification, leaving the parent free to keep working, while a mutable agent's
task runs inside the turn. An explicit value always wins. A *defaulted*
background task that cannot detach because background capacity is full runs in
the foreground instead of failing, and its result says so; an explicit
`background: true` there is still an error.

"Free to keep working" lasts as long as the turn does: a detached task's
cancellation is a child of the turn that spawned it, so aborting that turn
(Esc, or cancel-all in the pending manager) stops its detached tasks too. The
abort pauses notification delivery, so their `was cancelled` results are held
until the next message instead of starting a turn on the spot. Tasks spawned by
an earlier turn are not affected.

Workspace rule, one sentence: mutable work runs in worktrees and never
collides; read-only work runs in place, sees everything (uncommitted changes
included), blocks nothing, and accepts that the tree may shift while it looks.
Reads are advisory — a running explore never delays the parent's own edits or
builds — while two mutable tasks in one checkout remain impossible, and the
edit gate catches stale writes on the mutating side. The one wait that can
still happen — anything mutating while a same-checkout mutable task runs:
`bash`, `service`, `edit`/`write`, or another mutable task — names itself in
the tool row: "waiting for the workspace — a mutable task holds it".
(A detached task has no tool row to write that into — it is nobody's
blocked call — so it says it on its agents-panel row instead:
`· waiting for the workspace`, until the lease is its.)

Children share the parent's secrets, not a copy of them. A child's `bash`
asks the same person through the same prompt — and the prompt names the
child, `bash (reviewer subagent) wants GITHUB_TOKEN` — but the answer
lands in one shared set: "allow for this session" covers the root and
every subagent at every depth until ilar exits, and "always" is written
to the store for that tool. The `sudo` tool and its password work the
same way; a child gets the tool only when the session has it. Details in
[secrets](secrets.md#granting-a-use).

To have something *looked at*: save the image, then spawn a task with
`model` set to a vision model and point it at the file — the child's
`read` returns the picture itself, not just a description.

A task's session outlives the call. Every task result names it
(`task_id: <uuid>`), and passing that id back as the task tool's `task_id`
resumes that subagent with its context intact — a follow-up question costs a
sentence instead of re-explaining the scope to a fresh agent. Resuming is
guarded: the persisted agent, parent session and workspace must match, and a
task that is still running refuses a second driver. The read-only `tasks` tool
lists the current session's tasks (id, agent, model, how it stands, age,
opening prompt, what it said, and any messages still waiting for it) so the
agent can find the one worth resuming. How it stands is one of `running`,
`finished`, `cancelled`, `failed`, `stalled` or `aborted` — the same verbs
the task's notification used — and only a finished task has a `result:`; a
stopped one shows its last words as `partial:`, so a task killed with its
parent's turn is never mistaken for one that answered. A finished task's
result reaches its parent once, as a notification that may be held for a
while (an aborted turn holds its children's results until the next message);
until it lands, the listing says `result not delivered to you yet` and
carries the result itself (up to 8000 characters), so no second run is
needed to read it. The
ending is written to the task's own log as well, where the transcript shows
it as one line instead of simply stopping.

`task_message` talks to a task by id — one verb whether it is running or
finished, and the sender never needs to know which. A running background task
receives the message at its next step, exactly the way a steer reaches the
root turn, and keeps its own result path; a finished task is resumed from its
transcript with the message as its prompt, worktree and agent recovered from
its own metadata. A message the task's turn ended before reading is not lost:
it heads the prompt of that task's next resume, and the `tasks` listing shows
it as pending until it is actually seen. In the transcript, a delivered
message appears inside the child's rows at the moment the child saw it. On wide terminals an `agents` panel in the sidebar shows what
is in flight right now — description, agent, a `bg` marker for detached work,
and a live elapsed time — and disappears when nothing is running. A row that
is not working says why: `· waiting for the workspace` while it queues for a
mutable lease, `· quiet 45s` once a detached task has made no progress for a
while (the [stall watchdog](configuration.md#the-stall-watchdog) stops it at
600 s). Two other kinds of row share the panel: ✉ a result being delivered to
a session, and ⚙ a background `bash` job, which has no session to open. The
title counts each kind separately (`agents (2) · 1 job · 1 delivering`).
Clicking an agent's row opens its transcript over the screen, where Enter
messages it and **Ctrl-G** twice cancels it; see
[the interface](interface.md#talking-to-a-focused-agent).

## Skills

Skills are Markdown files in `${ILAR_CONFIG_DIR:-~/.config/ilar}/skills/` and
`./.ilar/skills/`. Project skills override user and built-in skills with the
same parsed name. Skill frontmatter supports `name`, `description`, and
`triggers`; root sessions list skill names and descriptions in the system
prompt and load full bodies on demand through the `skill` tool. The
directories are scanned once per start for names and descriptions; a body
is read when its skill is loaded, so an edit lands on the next load, and a
skill written mid-session loads by name at once. A skill file may be at most
256 KiB. Trigger cue phrases are included in the system-prompt listing so
the model invokes the skill when they match the task. In the TUI, typing `/` shows inline completion
for skills and built-in commands (Tab completes, Enter submits a fully typed
name); `/<skill-name> [arguments]` invokes a skill directly, and the palette's
"Invoke skill…" entry opens a picker.

## Commands

Commands are markdown whose body *is* the prompt. Unlike skills they are
never listed in the system prompt and the model can never invoke one:
`/name args` substitutes `$ARGUMENTS` (or `$1`, `$2`, …) into the body
and submits it directly. Put them in `~/.config/ilar/commands/` or a
project's `.ilar/commands/`.

```markdown
---
description: Address Greptile PR comments
---
Address Greptile feedback on the current pull request.

Command arguments: $ARGUMENTS
```

Frontmatter is TOML or YAML, so opencode and Claude Code command files
work unchanged. `$` is otherwise left alone — `$(date)`, `${HOME}` and
`$NAME` pass through — but `$` followed by a digit is always a
placeholder, so an unmatched one expands to nothing rather than staying
literal. A command sharing a name with a skill shadows it; `goal` is
reserved for the built-in.

Four more keys change *where* the prompt runs:

| Key | Effect |
| --- | --- |
| `subtask: true` | Run the body as a detached task instead of a turn in this session, under `build` unless `agent:` says otherwise. Its result lands here as a completion notification, and the agents panel shows it while it works. |
| `agent: <name>` | Which agent runs it — `build`, `explore`, or one of your own. Implies `subtask: true`, since an agent name means nothing anywhere else. Naming an agent that does not exist is refused when you invoke the command, listing the ones that do. |
| `model: <id>` | Override the model for this invocation only; the previous model comes back when the turn (or task) ends. |
| `variant: <name>` | The reasoning variant to go with `model`. |

```markdown
---
description: Survey the API surface
agent: explore
model: zai/glm-4.7
---
List every public entry point under $1 and what it is for.
```

## Image generation

With the openai provider configured — a ChatGPT login or an API key —
and unless `providers.openai.image_gen = false`, the model has an
`image_gen` tool: `{prompt, size?, quality?,
reference_paths?}`. It posts to the account's images endpoint with
model `gpt-image-2` (the same call Codex makes), writes the PNG under
`<state dir>/images/<session>/<call>.png`, returns the path, and attaches
the image to the result so a vision model can look at what it made. In a
session whose model takes no images the file is the whole result, and the
result says so rather than promising an attachment that is not there.
Reference images (up to five, resolved against the working directory)
turn the call into an edit; edits go through the ChatGPT backend's JSON
shape, so with an API key only generation is available today. Each call
is one image and is billed to that account.

## Services

The `service` tool manages long-running processes (dev servers,
watchers): `start {name, command}`, `status`, `logs`, `stop`. Services
keep running between tool calls, their combined output is retained in a
bounded buffer, and **everything is killed when the session ends or
switches** — no orphaned servers. They also survive compaction: the
handover summary has a Services section, and the summarizer is handed
the manager's live list so the next context knows which servers it
already owns. Running services appear in the sidebar
and in the pending manager (Ctrl-Q), where a confirmed `d d` stops them
all. Subagents share the session's services. Note that foreground bash
deliberately kills its process group on completion, so this tool is the
supported way to keep a server alive.

## Large tool output

`bash` returns at most ~30 KiB to the model: the tail of each stream,
with a guaranteed share for stderr. When a command says more than that,
up to 2 MiB per stream is written to
`${ILAR_STATE_DIR:-~/.local/state/ilar}/tool-output/<session-id>-<call-id>.txt`
and the result *opens* with the path, its size and line count — first
line, where both the model and the head-biased tool-result view can see
it — so the next step is a targeted `grep` or `read` instead of the
same command run again. When the spilled stdout is one complete JSON
document, the preview is its shape — the top-level keys with each
value's type and size — instead of a 30 KiB window into the middle of
it, and the hint says to `jq` the file. `grep` follows the same
discipline: matches past the preview budget go to the same directory
and the result opens with the pointer plus the head of the list.
`grep` and `glob` take absolute paths, which is what makes a spill
file reachable from any working directory. Spill files older than seven
days are removed at startup; filtering at the source (`jq`, `grep`,
`head`) is still cheaper than reading one back.

`bash` also takes `preview_bytes` — the output size the model expects.
On success the inline preview is capped at that (clamped to
1 KiB–30 KiB, lower-only) while the full output still spills, so a
surprise flood costs a declared budget instead of the full preview. A
failing command ignores the declaration: the error output arrives at
full size, because a truncated diagnosis just causes the command to be
run again.

## MCP

ilar deliberately ships no built-in MCP client. The built-in `mcp-via-cli`
skill teaches the agent to drive MCP servers through the external
[`mcptools`](https://github.com/f/mcptools) CLI instead (stdio and HTTP
servers, config discovery from common `mcp.json` locations). MCP servers run
outside ilar with whatever access your sandbox grants; ilar adds no credential
handling.
