# v0.3.0 release notes

The text of the annotated `v0.3.0` tag (2026-09-16, `adad113`), kept
here because a tag message is hard to read after the fact.

`v0.2.0` was never tagged: the line stopped at `v0.2.0-rc5` on
2026-08-29 and the version string sat at `0.2.0` for another 409
commits. Its draft notes are folded in below rather than released on
their own.

---

ilar v0.3.0

A personal coding agent in Rust: single binary, TUI-first, with OpenAI
(Responses, API key or ChatGPT OAuth), z.ai, OpenCode Zen and Go, and
any OpenAI-compatible endpoint of your own.

Secrets the model never sees. Keys used to sit in ilar.toml or the
environment, and the bash tool handed its whole environment to every
child. There is now a store of named values, a `secrets` argument on
bash and service naming what a command may see, and a grant protocol —
a prompt over a channel, a one-shot reply — that each driver answers
its own way: a modal in the TUI, `/grant` in a chat, a refusal with the
CLI line under `ilar exec`. Redaction reaches the session store, not
just the screen.

Root, asked for. A `sudo` tool runs one command as root after the
person has read it, with the same once/session/always answers, and
takes the password in the prompt rather than storing it.

The assistant. `ilar-gateway` drives a live session per chat and
answers on Delta Chat through deltachat-rpc-server's stdio. The model
replies by calling a `message` tool, so it can send several, attach
files, address another chat it knows, or stay silent; subagent
completions come back as follow-up turns.

Sessions. `ilar --view <id>` watches one read-only and live, with no
provider and no lease. The listing is fast again — a summary cache, a
per-directory pointer that answers `--continue` without a scan, empty
sessions swept on quit — and a bare `ilar` offers this directory's last
session with its tail ghosted above the prompt: Enter resumes, typing
starts fresh.

Providers and models. OpenCode Zen and Go, cataloged from live probes
with the wire known per model; endpoints that discover what they serve;
reasoning variants per model in the picker.

Everyday. Images bounded before decoding and priced the way a model
bills them; previews that are the request rather than a guess at it;
tools that refuse what cannot be meant; a running job on the agents
panel; three ways to stop a runaway; and a thirty-issue UX sweep out of
one weekend of real use.

`ilar serve` and its web view are behind an off-by-default Cargo
feature until the terminal agent is in shape.

Pre-alpha: no sandbox, and no permission system for tools beyond the
secret and sudo grants. Run it inside one.

---

## Folded in: the unreleased v0.2.0 notes

Recovered from the premature `v0.2.0` tag (created 2026-08-23 at
`62bcc4c`, deleted 2026-08-27 because it predated its own release
candidates), and written before the Milestone 12 health sweep, `ilar
serve`, the edit gate, output spill, task steering and custom models.
What it lists is the foundation everything above stands on, still true
in 0.3.0:

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
