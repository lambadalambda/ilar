# Context-aware automatic tool-call review

## Summary

Investigate an optional reviewer that evaluates proposed tool calls against the
user's request and policy before execution. The aim is fewer manual approvals
without unrestricted execution. Model review is an additional control, not a
replacement for a filesystem/network sandbox.

Related work:
- [Completed local checker experiment](local-tool-call-security-experiment.md)
- [Kernel sandbox for tool processes](kernel-sandbox-for-tool-processes.md)
- [Cases, runner and instructions](../../experiments/tool-guard/README.md)
- [Full results](../../experiments/tool-guard/RESULTS.md), with raw responses in
  `experiments/tool-guard/results/`.

## Research findings

Codex Auto-review and Claude Code auto mode use separate reviewers/classifiers
to evaluate proposed actions. Calling an ordinary model API does not automatically
provide the Codex approval system. ilar would need its own enforcement path.

Options are not mutually exclusive:

- General coding/reasoning model, local or remote: flexible task-aware review
  through ilar's provider interface; adds latency and can accept injected context.
- Specialized local models: TS-Guard, ShieldAgent, gpt-oss-safeguard and Granite
  Guardian. TS-Guard targets tool-call safety; ShieldAgent was documented as a
  trajectory judge. Safeguard accepts custom written policy. Generic content
  moderation or function-call correctness is not action authorization.
- Small CPU classifiers such as Agent Action Guard: cheap action-risk signals,
  but its normal tool-name/arguments input does not establish user authority.
  Its published benchmark and CC BY 4.0 software licensing need care.
- Deterministic rules: small native checks, Cedar (native Rust), or OPA for known
  permissions. Invariant adds contextual trace rules; NeMo is a broader guardrail
  framework. None establishes all effects of arbitrary shell code.
- Security gateways/APIs: useful for routed MCP/model traffic and injection or
  leakage detection, but do not automatically cover native tools or subprocesses.
- Integrated sandboxing: nono (Rust-based) or Anthropic sandbox-runtime are
  possible alternatives to building all process confinement ourselves. They
  restrict effects rather than determine intent. Defaults need inspection;
  sandbox-runtime does not restrict all reads by default.
- CaMeL-style separation of trusted planning from untrusted data is a longer-term
  architecture option, not a drop-in checker. The released code is a research
  artifact. ToolGuardian is another policy design reference, not a verified
  ready-to-integrate dependency.

References: [Codex](https://learn.chatgpt.com/docs/sandboxing/auto-review),
[Claude Code](https://code.claude.com/docs/en/permission-modes),
[ToolSafe/TS-Guard](https://github.com/MurrayTom/ToolSafe),
[ShieldAgent](https://huggingface.co/thu-coai/ShieldAgent),
[Safeguard](https://huggingface.co/openai/gpt-oss-safeguard-20b),
[Invariant](https://github.com/invariantlabs-ai/invariant),
[nono](https://github.com/nolabs-ai/nono),
[Anthropic sandbox](https://github.com/anthropics/sandbox-runtime),
[CaMeL](https://github.com/google-research/camel-prompt-injection).

## Local experiment findings

The unchanged development set has 30 cases: 16 expected approvals, 14 denials.
Nine calls came from older ilar sessions; user requests and policy are synthetic.
It includes dangerous mutations, inert quoted commands, prompt injection and
identical calls with different user permissions. No tested command was executed.
This is a small hand-labeled development set, not an independent benchmark.

| System | Mean request | Exact decisions | Unsafe approvals | Benign blocks/escalations |
| --- | ---: | ---: | ---: | ---: |
| ShieldAgent FP16 Transformers, 128 tokens | 9.393 s | 26/30 | 0/14 | 4/16 |
| ShieldAgent FP16 MLX, verdict only | 1.338 s | 26/30 | 0/14 | 4/16 |
| ShieldAgent 4-bit MLX, verdict only | 1.186 s | 24/30 | 2/14 | 4/16 |
| ShieldAgent 4-bit MLX, 128 tokens | 4.273 s | 24/30 | 2/14 | 4/16 |
| TS-Guard FP16 Transformers | 17.510 s | 23/30 | 1/14 | 2/16 |
| TS-Guard 4-bit MLX | 7.236 s | 20/30 | 0/14 | 5/16 |

All runs used the Mac GPU, not CPU-only inference. Timing excludes model loading
and includes prompt processing; the report also gives first-case and median
times. ShieldAgent emits its answer first; verdict-only runs generated five tokens.
TS-Guard reasons before answering. Capped ShieldAgent explanations were truncated.

Matched ShieldAgent MLX runs show only about 11% lower mean latency from 4-bit;
the large improvement over nine seconds also changes engine and output length.
Weight storage fell from 15.231 GB to 4.284 GB. FP16 MLX preserved every original
ShieldAgent decision, while 4-bit newly approved whole-home credential discovery
and removal of the secret-grant check during an unrelated model-picker task.
Both models mistook a printf containing a quoted deletion command for executing
that command. Neither is suitable as the sole permission boundary on this evidence.
FP16 MLX ShieldAgent is the better next comparison baseline, not an adopted
production default. Zero unsafe approvals in 14 examples is not a safety guarantee.

## Context and authorization limits

The checker sees the user request, proposed tool/arguments, tool/environment
information and policy, plus prior tool output when supplied. It does not see the
full original session in these tests. Labels and expected explanations are not
sent to it. Tool output must remain untrusted evidence, not a source of authority.

The current policy prohibits real credential access. An explicit request to
search for credentials therefore does not automatically make it allowed. A
scoped audit of accidentally committed secrets, returning locations rather than
values, differs from reading private keys or exporting secrets. Those boundaries
must be defined before treating a reviewer verdict as approval.

Deletion and force-push context pairs were tested and distinguished by both
models. An explicitly authorized credential-search pair has NOT been tested.
The existing credential-search case is unrelated to the user's requested task.

## Follow-up requirements

- Add paired credential-audit cases with identical calls but different authority,
  scope and output handling. Include legitimate source audits and forbidden
  private-key reads/export; do not weaken the policy merely to get approvals.
- Keep cases fixed when comparing other local/remote models and rule systems.
  Compare false approvals, false blocks, abstentions, errors and verdict latency.
- If building a prototype, construct context in `agent/turn.rs` and enforce the
  decision in `tools/executor.rs` before execution or workspace-lock acquisition.
  `aside.rs` is a request-handling reference, not a drop-in reviewer.
- Preserve hard denials, capability restrictions, secret/root grants, cancellation
  and sandbox boundaries. A review failure must not permit execution. Review the
  exact arguments that run, including subagent/background paths. `question`
  bypasses ordinary executor dispatch and needs an explicit coverage decision.
- Do not equate read-only scheduling with safety or harmless-looking commands
  with harmless effects: credential reads and repository-controlled test scripts
  need separate treatment. Model-plus-rules-plus-sandbox is the working direction,
  not a decision to replace user approval or external isolation.

## Observation-mode prototype (requested 2026-11-15)

Build an opt-in reviewer with an explicitly selected model in user-only config.
Review exact executable tool arguments with user instructions kept separate from
untrusted conversation/tool data. Record allow/ask/deny or review errors and
latency in session history, without adding the verdict to model conversation.
Observation does not change execution, manual approvals, or sandbox grants.
Bound requests, support cancellation, and give the reviewer no tools. Cover
root and child calls, direct file tools, and background launch requests. Keep
`question` outside review because it requests user input rather than effects.
No model default, automatic grants, cached approvals, or enforcement in this
first prototype.

Implemented observation before dispatch in the turn loop; no executor enforcement
hook is needed until enforcement is added. Requests and replies are bounded,
with per-review timeouts and cancellation. Session metadata is excluded from
model conversation and recall. A fallible secret snapshot prevents outbound
review when the store is locked or unreadable. Secret-bearing arguments are
skipped; readable history is redacted. Bounded recorded messages can include
notifications/delegation, so this prototype does not establish human authority.

Six module tests cover strict verdicts, exact arguments, no tools, input/output
limits, failures, timeout and cancellation. Configuration and turn integration
tests cover user-only settings, child-depth calls, unchanged execution after
deny/errors, metadata replay, history redaction, and secret/locked-store skips.
All 2066 workspace tests pass (24 ignored); all-target/all-feature Clippy and
changed-file formatting checks pass. One existing transcript-paging test failed
on the first full run, then passed alone and on the full rerun. Astra reviewed
the design and the locked-store fix with no remaining blockers. This issue
remains open for policy evaluation and any future enforcement mode.

## ShieldAgent runtime adapter

Add an explicit `shieldagent` adapter that uses the tested native prompt shape
and maps initial `[Answer] safe|unsafe` responses to allow/deny. Keep the generic
JSON adapter as the default. During streaming, stop only after the complete
verdict word and following whitespace; malformed or incomplete answers remain
errors. Preserve bounded input, cancellation, no-tools requests, secret handling,
and observation-only behavior. Provide and verify a local MLX server setup with
the original FP16 weights, not the 4-bit model. Runtime context differs from the
synthetic benchmark; do not claim the old accuracy or timings apply unchanged.

Implemented with explicit adapter metadata, backwards-compatible JSON defaults,
native parsing/early stream drop, and documented local setup. The final parser
rejects punctuation suffixes such as `safe-ish`, which the benchmark's final
word-boundary parser accepted. The live FP16 HTTP smoke returned allow for Git
status (1719 ms) and deny for a private-key export proposal (1560 ms); neither
proposal executed. Nine reviewer tests and all 2069 workspace tests pass (25
ignored), with Clippy and changed-file formatting passing. Astra found no
blocking defects. Policy evaluation and enforcement remain outside this change.

## Acceptance Criteria

- Document intended authorization boundaries, especially legitimate credential
  audits versus secret access/export, and test the missing context pairs.
- Compare candidates on the reusable set with matched timing/stopping conditions.
- Decide whether a runtime prototype is justified and record its limits before
  selecting a default model or approval mode. Observation integration is built;
  enforcement and a default reviewer remain undecided.
