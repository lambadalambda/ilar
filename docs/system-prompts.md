# System prompts and session context

ilar keeps the instructions that define an agent separate from the conversation
stored in a session. This document describes what is sent to providers and when
that context is refreshed.

## Root-session prompt composition

When the TUI enters a root session, it assembles one system-prompt string in
this order:

1. ilar's built-in base instructions.
2. User instructions from `${ILAR_CONFIG_DIR:-~/.config/ilar}/AGENTS.md`, or
   `CLAUDE.md` when `AGENTS.md` is absent.
3. Working-directory instructions from `./AGENTS.md`, with the same
   `CLAUDE.md` fallback.
4. The names, descriptions, and trigger cues of the currently discovered
   skills.
5. The selected agent definition's prompt, when it has one.

Only the exact user configuration directory and current working directory are
checked for instruction files. ilar does not walk parent directories. Tool
schemas are also sent to the provider, but as separate request data rather than
as part of this system-prompt string.

## Refresh lifecycle

The root system prompt is built when a session runtime is entered. This occurs
when ilar starts, when a new session is entered, and when the session picker
switches sessions. Switching away and then back therefore rebuilds the prompt.
Restarting ilar and resuming a session rebuilds it too; the assembled prompt is
not stored in the session JSONL. A session persists its selected agent name and
model state, not the agent prompt. On rebuild, ilar uses the current definition
for that agent name while the session's persisted model continues to override
an edited agent default.

Within one active root-session runtime, the assembled prompt is reused for:

- every user turn;
- every provider step within an agentic turn;
- provider retries and failed-turn resume;
- notification-driven turns; and
- context estimation used by automatic and manually requested compaction.

The summarization request itself is a deliberate exception described under
[Compaction handovers](#compaction-handovers).

It is not recalculated before each turn or provider call. Editing `AGENTS.md`,
`CLAUDE.md`, skill metadata, or an agent definition while staying in the same
root session therefore does not update its active prompt. Restart ilar or
switch away from and back to the session to load those changes. Keeping the
prompt stable within a runtime also preserves a stable provider prompt-cache
prefix.

The TOML configuration itself is loaded when the application starts. Entering
a session rereads instruction files, the skill listing, and agent Markdown
files, but does not reload `ilar.toml` in the existing process. The skill
listing embedded in the prompt and the TUI's slash-completion inventory remain
snapshots. Each `skill` tool invocation independently rediscovers and reparses
the current skill inventory, so tool-side names, metadata, precedence, and
bodies can change immediately even while the prompt and completion list remain
stale.

## Subagents

A subagent builds its instruction-file portion for each explicit task start or
resume, using the child task's effective working directory. Notification-routed
follow-up turns rebuild it again. This matters for isolated worktrees: their
working-directory `AGENTS.md` or `CLAUDE.md` is read from the child workspace,
not blindly inherited from the root workspace.

The chosen subagent definition is taken from the agent inventory loaded for the
current root-session runtime, and its prompt is appended to those instructions.
Consequently, instruction-file edits can affect a newly started, resumed, or
notification-routed subagent turn immediately, while edits to agent definition
files require rebuilding the root-session runtime first. Subagents do not
inherit the root prompt's skill listing or receive the root `skill` tool.

A child prompt remains stable across the provider steps and retries within that
one subagent turn. A resumed task preserves its conversation in the child
session, but its prompt is assembled again for the new invocation.

## Compaction handovers

The provider call that generates a compaction handover is special: it receives
a dedicated summarizer system prompt, the portion of active conversation being
summarized, and no tools. It does not replace the normal prompt assembled for
that root or subagent turn. Later ordinary turns receive their normal prompt
again, including the applicable instruction files and agent prompt. Root
sessions support both automatic and manual `/compact` handovers; subagent
sessions use the same automatic compaction machinery but have no interactive
manual command.

A successful compaction appends a dedicated `Compaction` event to the
append-only session log. When ilar renders the active provider-request
transcript, the latest handover is represented as user-role content:

```xml
<compaction-summary>
...generated handover...
</compaction-summary>
```

The handover is not stored as an ordinary `UserMessage` event. Events before
the compaction boundary remain in the canonical JSONL for audit and recovery,
but are omitted from future provider requests. Manual compaction places the
boundary after the complete active history. Automatic compaction at turn start
summarizes history before the new user message and retains that turn; if one
agentic turn grows too large, compaction between settled provider steps can
instead retain a budgeted recent-step tail.

During live manual compaction, the TUI adds the handover as a system line and
leaves the already rendered scrollback visible. Reopening the session rebuilds
the view from the active boundary, showing the latest handover and any retained
tail rather than the older compacted events.

If the next retained or newly submitted message is also user-role content,
ilar coalesces it with the handover into one provider-neutral user message.
This preserves the strict user/assistant alternation required by providers.
