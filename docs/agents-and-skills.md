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

Built-in subagents are `build` (mutable, serialized per checkout) and `explore`
(read-only, safe for parallel repository inspection and review). Tasks can
override the child's model per invocation (`model` and `reasoning` on the task
tool — e.g. a cheap flash model for mechanical sweeps); omitted, the child uses
the agent definition's model or inherits the parent's model and reasoning. The
read-only `models` tool lists available models with context windows, pricing,
and reasoning variants so agents can choose informedly.

A task's `background` follows its agent when the call omits it: a read-only
agent's task (`explore`) detaches and reports back as a completion
notification, leaving the parent free to keep working, while a mutable agent's
task runs inside the turn. An explicit value always wins. A *defaulted*
background task that cannot detach — background capacity is full, or the caller
holds a workspace lease the child would outlive — runs in the foreground
instead of failing, and its result says so; an explicit `background: true` in
those spots is still an error.

To have something *looked at*: save the image, then spawn a task with
`model` set to a vision model and point it at the file — the child's
`read` returns the picture itself, not just a description.

A task's session outlives the call. Every task result names it
(`task_id: <uuid>`), and passing that id back as the task tool's `task_id`
resumes that subagent with its context intact — a follow-up question costs a
sentence instead of re-explaining the scope to a fresh agent. Resuming is
guarded: the persisted agent, parent session and workspace must match, and a
task that is still running refuses a second driver. The read-only `tasks` tool
lists the current session's tasks (id, agent, model, running or finished, age,
opening prompt, a snippet of the last reply, and any messages still waiting
for it) so the agent can find the one worth resuming.

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
and a live elapsed time — and disappears when nothing is running.

## Skills

Skills are Markdown files in `${ILAR_CONFIG_DIR:-~/.config/ilar}/skills/` and
`./.ilar/skills/`. Project skills override user and built-in skills with the
same parsed name. Skill frontmatter supports `name`, `description`, and
`triggers`; root sessions list skill names and descriptions in the system
prompt and load full bodies on demand through the `skill` tool. Trigger cue
phrases are included in the system-prompt listing so the model invokes the
skill when they match the task. In the TUI, typing `/` shows inline completion
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

## Services

The `service` tool manages long-running processes (dev servers,
watchers): `start {name, command}`, `status`, `logs`, `stop`. Services
keep running between tool calls, their combined output is retained in a
bounded buffer, and **everything is killed when the session ends or
switches** — no orphaned servers. Running services appear in the sidebar
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
same command run again. `grep` and `glob` take absolute paths, which is what makes that
file reachable from any working directory. Spill files older than seven
days are removed at startup; filtering at the source (`jq`, `grep`,
`head`) is still cheaper than reading one back.

## MCP

ilar deliberately ships no built-in MCP client. The built-in `mcp-via-cli`
skill teaches the agent to drive MCP servers through the external
[`mcptools`](https://github.com/f/mcptools) CLI instead (stdio and HTTP
servers, config discovery from common `mcp.json` locations). MCP servers run
outside ilar with whatever access your sandbox grants; ilar adds no credential
handling.
