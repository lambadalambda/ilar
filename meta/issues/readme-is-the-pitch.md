# The README is the pitch; details live in docs/

## Summary

The README grew into a reference manual: 372 lines of config tables,
status-line anatomy and OAuth steps before a potential user learns why
they would want the tool. Meanwhile several shipped features are
documented nowhere: the cross-session content search, `/btw` asides,
session topics and the terminal title, and the pending-messages strip.

## Requirements

- The README reads as a first look: what ilar is, why it is
  interesting, the safety warning (which stays prominent), install,
  a quick start, and pointers into docs/.
- The reference detail moves to topic files under docs/ —
  interface, configuration, sessions, agents-and-skills — joining the
  existing checkpoints and system-prompts docs.
- Every shipped feature is documented somewhere: the audit's missing
  four get sections.

## Acceptance Criteria

- No feature is documented only in the help overlay.
- The README fits a screen or two and links to everything else.

## Milestone

11 — Beyond the terminal

## Outcome

The README is now the pitch: conviction line, the safety warning
(unmoved — it is part of an honest first look), a highlights list that
sells the actual differentiators (steering, forget-nothing sessions,
grep-your-history switching, tree-inclusive rewind, asides, goal mode,
cache frugality), install, quick start, a documentation table, and the
trimmed design principles. 372 lines down to ~130.

The reference detail moved to four new topic files —
docs/interface.md, docs/sessions.md, docs/configuration.md,
docs/agents-and-skills.md — joining checkpoints.md and
system-prompts.md. The four previously undocumented features
(cross-session search, /btw, topics + terminal title, the pending
strip) got sections in interface.md and sessions.md.
