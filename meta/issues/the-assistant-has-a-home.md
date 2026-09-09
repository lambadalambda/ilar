# The assistant has a home

## Summary

The assistant's identity is scattered: `SOUL.md`, skills and agent
definitions come from `~/.config/ilar`, shared with the terminal
agent, while memory, workspace, routes, cron and the Delta Chat
account live under `~/.local/state/ilar/gateway/`. Once the assistant
writes its own skills and memory (the learning issues), those are its
state, not configuration, and a review pass must not be able to edit
the skills coding sessions rely on. One home, as Hermes has
`~/.hermes/`.

## Requirements

- `gateway.home`, defaulting to `<state dir>/gateway`, holds
  `SOUL.md`, `skills/`, `agents/`, `memory/`, `workspace/`,
  `routes.json`, `cron.json`, `inbox/`, `deltachat/`.
- A gateway session reads its user directory — instruction file,
  skills, agents, commands — from the home, through one runtime
  option. Providers and model keys stay in `ilar.toml`.
- No fallback to `~/.config/ilar`: an assistant without a `SOUL.md`
  gets the base prompt and nothing else; the docs say to write one.
- `ilar-gateway prompt` and the docs follow.

## Acceptance Criteria

- A test: a `SOUL.md` and a skill in the home are in the prompt, and
  the config directory's `AGENTS.md` and skills are not.
- Tenco keeps working with no config change (the default is where
  everything already is).
