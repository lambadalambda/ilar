# v0.2.0 release notes (draft)

Recovered from the premature `v0.2.0` tag (created 2026-08-23 at
`62bcc4c`, deleted 2026-08-27 because it predated its own release
candidates). Written before the Milestone 12 health sweep, `ilar
serve`, the edit gate, output spill, task steering, and custom
models — extend, don't reuse as-is, when tagging the real v0.2.0.

---

ilar v0.2.0

A personal coding agent in Rust: single binary, TUI-first, OpenAI
(Responses, API key or ChatGPT OAuth) and z.ai providers.

Since the loop first ran end to end:

- Flow: steer a running turn, queue messages, goal mode, prompt history,
  session list/resume/fork, background jobs and subagents with completion
  notifications.
- Commands and skills: markdown files from the user config dir and the
  project's .ilar/, including Claude and opencode layouts, with per-command
  model, agent and subtask frontmatter.
- Transcript: markdown with syntax-highlighted fences, diffs, tables,
  hierarchical tool and subagent activity, click-to-expand, search,
  select and copy.
- Colour: surfaces and syntax slots per theme, damped chrome, and fifteen
  palettes including Monokai, Dracula, Gruvbox, Solarized, Tokyo Night,
  Catppuccin, One Dark and Rosé Pine. Carbon is the default.
- Keys: Ctrl-C interrupts, Ctrl-D quits.
- Prompt caching on the Codex backend, which needed session identity
  headers rather than the documented cache key alone: measured 2/10
  cache-eligible steps hitting before, 10/10 after.
- The event loop's schedule is a tested seam, and tool scheduling,
  compaction, session replay and provider streaming are covered by the
  suite.

Pre-alpha: no sandbox, no permission prompts. Run it inside one.
