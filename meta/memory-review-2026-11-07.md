# Memory review, 2026-11-07

Source: Anthropic, [The components of agent memory](https://www.anthropic.com/research/components-of-agent-memory), published 2026-11-06.

## Assessment

The storage and retrieval design fits the article. The main missing part is a reliable way to check persistent claims against their original evidence. Gateway maintenance is more developed than terminal maintenance. Three confirmed write-boundary problems should come before new memory mechanisms.

The article reports experiments across models and tasks, not a specification for Claude Code. Its wiki results do not justify adding a wiki to a small project store: some tasks gained nothing, and one wiki grew larger than its sources. Likewise, its prompt experiments justify testing our instructions, not deleting them on assumption.

## What already fits

| Article component | Current implementation | Assessment |
| --- | --- | --- |
| Small injected context | Core files capped at 6,000 and 2,500 characters; opening index capped at 200 notes and 25 KiB | Bounded; bodies are read on demand. These are separate budgets, not a 25 KiB total. |
| Notes written during work | Root memory tools, amend/forget, terminal standing prompt and pre-compaction flush | Present. The flush saves context; it is not cross-session source validation. |
| Search before full reads | BM25 over note bodies and headlines, then `memory_get`; optional model selector | Present. Automatic selector recall remains off by default in both drivers. |
| Background maintenance | Gateway idle review and weekly promotion, demotion and correction prompt | Partial. Terminal stores have no corresponding consolidation job. |
| Searchable original experience | Append-only session logs, current-session `history`, person-facing TUI cross-session search | Evidence survives, but the model-facing retrieval path is incomplete. |
| Avoid redundant state | Instructions exclude transient progress and facts already available from files or search | Fits the article's concern about stale copies. This is a task-specific choice, not a universal ban on plans. |

Sources: `crates/ilar/src/memory.rs:32-55,118-132,373-418,953-1013,1095-1142`; `crates/ilar/src/runtime.rs:604-642`; `crates/ilar/src/compaction.rs:490-507`; `crates/ilar/src/config/toml.rs:922-949`; `crates/ilar-gateway/src/config.rs:76-95,134-141`; `crates/ilar-gateway/src/weekly.rs:65-78`.

Other useful safeguards: amendments that change a body must also supply a summary; forgotten notes stay recoverable outside the live archive; recalled notes are explicitly unverified claims, not instructions. Claude imports preserve bodies and stable identity, and do not restore forgotten notes on a second import (`memory.rs:957-1013,626-671`; `claude_memory.rs:173-220`). Import is a copy, not ongoing synchronization or evidence validation.

## Confirmed problems

### Shared stores can lose successful writes

`MemoryStore` has an instance-local mutex. Separate terminal sessions and worktrees can name the same store but own different mutexes. Core writes read a whole file and replace it through an atomic rename; atomic replacement does not serialize the read-modify-write sequence (`memory.rs:136-150,234-246,773-785,821-843`). The same limitation applies to competing amendments of one note.

An isolated diagnostic used two independent stores pointing at one temporary directory. In 50 barrier-synchronized rounds, 100 unique core adds returned success but only 50 lines remained. No real memory was used or changed.

Track: [Serialize shared memory writes](issues/serialize-shared-memory-writes.md).

### Idle review bypasses the memory off-switch

The gateway removes memory from the chat runtime when `memory.enabled` is false, but idle review checks only review enablement and seat type. With approval off, which is the default, its plan writes directly to the store (`driver.rs:934-949`; `gateway.rs:1689-1693,1765-1789`). With approval on, the plan is staged; `/approve` can then apply it without a memory-enabled check (`gateway.rs:1186-1192`). A qualifying review can therefore create core entries or notes while memory is disabled, either immediately or after approval. Skill review can remain independent; memory edits need their own gate.

Track: [Memory off stops review writes](issues/memory-off-stops-review-writes.md).

### Group handovers enter shared daily memory

The group runtime withholds personal memory tools and context. However, completed and aborted turns pass compaction summaries to `keep_handovers`, and `note_handover` checks memory enablement but not whether the seat is private (`gateway.rs:772-782,1793-1810`). The weekly private job is asked to read those shared daily notes (`weekly.rs:65-78`).

This confirms group-content retention in the shared store when the write succeeds. Promotion into private memory is possible, not demonstrated or guaranteed. It is not evidence of private-to-group disclosure.

Track: [Group handovers stay out of personal memory](issues/group-handovers-stay-out-of-personal-memory.md).

## Main design gaps

### Maintenance does not close the evidence loop

The article's most useful result is correction against original transcripts, not merely rearranging notes. Our weekly prompt works from daily handover summaries and memory search. Daily notes cover compactions, not every conversation. Idle review sees the current effective transcript through an aside, but cannot call tools to inspect an older note or find its source (`weekly.rs:65-78`; `gateway.rs:1793-1810`; `driver.rs:763-780`; `crates/ilar/src/aside.rs:20-24,103-143`).

Notes have an ID, kind, title, summary, date and body, but no automatic source-session/event reference or verification state. `Why` and `How to apply` explain a rule; they do not establish how it was verified (`memory.rs:46-55,327-334,1401-1403`).

A focused next step is a bounded validation pass: select a few important or change-prone notes, retrieve their source evidence within the correct project/private scope, correct or retire contradicted claims, and record the evidence checked. A blanket scan of every note is neither required by the article nor justified by this review.

### Terminal consolidation and evidence retrieval are incomplete

[Terminal stores get consolidated](issues/terminal-stores-get-consolidated.md) is already open. The root agent's `history` tool is deliberately restricted to its own session; TUI cross-session search is available to the person, not as that model tool (`crates/ilar/src/tools/history.rs:190-203`; `crates/ilar/src/recall.rs:436-445`; `crates/ilar-tui/src/main.rs:3501-3514`). General file tools are not an equivalent scoped retrieval interface.

Delegated agents do not automatically receive the root's memory tools or memory recall. Relevant facts must be in their brief. This is a boundary to preserve or design around, not necessarily a defect (`crates/ilar/src/subagent.rs:941-953`; `crates/ilar/src/runtime.rs:830-855`; `crates/ilar/src/tools/mod.rs:1499-1512`).

### Quality evidence is promising but limited

The 2026-11-05 DEVLOG records 60 prompts, with useful notes on 23: the default selector had 59% precision, 33% recall, and stayed quiet on 36 of 37 no-help cases. Keeping automatic recall off pending checks is sensible. These are recorded results, not measurements rerun for this review.

The current evaluation is a useful selector comparison, not proof of improved task outcomes:

- `collect` reads current note files and filters by creation date. Since amendments preserve that date, later edits can enter earlier cases; forgotten notes are absent. This does not reconstruct the historical store (`scripts/recall-eval.py:87-123`; `memory.rs:953-985`).
- The judge supplies model-generated labels. Despite the full-body description, the script sends only the first 800 body characters. Human spot-checks are requested but not implemented by the script (`recall-eval.py:8-14,130-159`).
- Offline selection does not reproduce the runtime's selector history or held-note filtering, and ages are relative to the newest candidate's date rather than the message timestamp (`recall-eval.py:192-209`; `memory.rs:474-478`).
- It reports agreement with labels and latency, with no held-out validation protocol or downstream task replay (`recall-eval.py:184-226`).

Extend [Recall is measured before it is on](issues/recall-is-measured-before-it-is-on.md) with snapshot-correct cases and human checks before relying on those numbers. For consolidation, compare no maintenance, current maintenance and source-backed validation on later task success, false corrections, token cost and archive growth.

Our detailed write instructions may help some models and constrain others. Compare them with shorter purpose-led guidance on representative tasks; do not assume Anthropic's specific "record only failures" result transfers to our rules.

## Lower-priority observations

These do not justify expanding this review into fixes:

- History search has no pagination beyond 40 matches, and reading around an event clips each entry to its first 400 characters. A deep hit can be hard to verify (`crates/ilar/src/tools/history.rs:15-18,212-221`; `crates/ilar/src/recall.rs:213-276`).
- The cross-session prefilter documents a Unicode false-negative case (`recall.rs:350-378,467-481`).
- Review resets the episode before applying or staging a plan, so failures are not retried from that episode. A separate, narrow lost-counter race after releasing the seat lock is plausible on multiple workers but was not reproduced (`gateway.rs:1748-1789`; `driver.rs:767-780`).
- Review suppression after any successful memory write is an intentional duplicate-avoidance trade-off: unrelated unsaved facts in the same episode can be missed (`gateway.rs:1721-1735`).
- Core and index text enter the system prompt. Incorrect or adversarial memory deserves an evaluation of its own; this review did not demonstrate a prompt-injection exploit (`crates/ilar/src/runtime.rs:617-640`).

## Verification and recommended order

All focused tests passed:

| Command | Passed |
| --- | ---: |
| `cargo test -p ilar --test memory --test claude_memory` | 13 |
| `cargo test -p ilar --lib memory::` | 30 |
| `cargo test -p ilar-gateway --lib` | 83 |
| `cargo test -p ilar-gateway --test gateway` | 48 |

The 174 passing tests establish mechanics, not semantic memory quality. The temporary lost-update diagnostic demonstrates a gap outside their coverage. No model APIs or private memory were used. No runtime code changed; red/green implementation work and the full workspace gate were outside this review.

Recommended order: fix shared-store writes and the two gateway write boundaries; design source references and scoped evidence retrieval with terminal consolidation; then measure correction quality and later task outcomes. Keep the existing file-based tiers. Do not add a wiki, vector database or automatic recall rollout just to match an article.
