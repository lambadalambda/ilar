# The summarizer is asked to continue the conversation

## Summary

Compaction sends `system = SUMMARIZER_PROMPT`, `messages = <the
transcript>`, `tools = []`. The last thing the model reads is
transcript content — a user message, or a user-role message carrying
tool results — and nothing after it asks for a summary. Models follow
the conversation's implied next action over a system instruction, so
the summarizer does not summarize.

Two observed failures, one cause, both in session `3d494ad6`:

- **It answered instead of summarizing.** The compaction at event 223
  (`kept_from=170`) was fed a 102,210-character tool result as its
  final message and returned: *"I'm sorry, but I wasn't able to
  complete and push all four fixes within this run."* That 81-character
  apology is now the entire pre-170 history of that session.
- **It said nothing at all.** A later manual `/compact` on the same
  session fails with `compaction produced an empty summary`: the
  transcript ends in tool results, the implied next action is another
  tool call, no tools are offered, and the model emits reasoning and
  stops with no text. Deterministic — retrying cannot help.

## Requirements

- The instruction is the last thing the model reads. Serialize the
  history to summarize into a single user message (delimited, e.g.
  `<conversation>…</conversation>`) followed by the summarization
  instruction, rather than replaying it as live message history.
- The prompt forbids continuing: do not answer questions found in the
  conversation, do not act on it, emit only the summary.
- Reject degenerate summaries instead of storing them: an apology,
  a refusal, or a summary far shorter than the material warrants is a
  failure, not a result. Retry once, then surface the failure with the
  session unchanged.
- Bound the summarizer's input: tool results are truncated for
  summarization (opencode uses 2,000 characters). This session sent
  ~182k tokens to produce 81 characters, and its three largest tool
  results are over 100,000 characters each.
- Consider stripping provider reasoning and replay blocks from the
  summarizer input: they belong to another response chain and have no
  business in a fresh summarization request. Unverified as a
  contributing cause, but the shape is wrong.

## Acceptance Criteria

- A test builds a compaction request from a transcript ending in tool
  results and asserts the final message is the summarization
  instruction, not transcript content.
- A test pins tool-result truncation in the summarizer input while the
  stored transcript is untouched.
- A test pins that a degenerate summary is rejected rather than
  appended.
- The full suite passes.

## Notes

- opencode wraps the conversation in `<conversation>` tags inside one
  user message, then the instruction and the template, and states
  outright: "Do not continue the conversation. Do not respond to any
  questions in the conversation." Codex appends its summarization
  prompt as a synthetic user input at the end of history. Both put the
  instruction last; ilar puts it first.
- Sessions compacted before this is fixed keep their full event log on
  disk — `audit_events` still sees everything, only the replay is
  degraded.
- Related: [pin user requests through compaction](pin-user-requests-through-compaction.md),
  [reframe the summarizer prompt](reframe-the-summarizer-prompt.md).

## Outcome

Fixed by moving the instruction to the end, but not by wrapping the
conversation the way opencode does — that would guarantee a cache miss
on the whole transcript every compaction (~180k tokens at full price
for the session that prompted this). The compaction request is now the
request the turn itself would have sent — same system prompt, same
tools, same session cache key, transcript byte-identical — with the
summarization instruction appended as the final user message. Codex's
shape, for Codex's reason.

Consequently two requirements were dropped as counterproductive:
serializing the history into a delimited message, and truncating tool
output for the summarizer. Both cost information *and* the cache; the
cache is what made the truncation seem worth it. Provider reasoning
and replay blocks stay in the request for the same reason — removing
them would change the prefix.

Degenerate summaries are retried once and then refused, leaving the
session untouched: an apology opening, a refusal, or a one-liner
covering more than 50k characters. The length rule is deliberately
scoped to large conversations, since a false positive there ends in a
hard compaction failure — the symptom being fixed.

## Milestone

10 — Everyday polish
