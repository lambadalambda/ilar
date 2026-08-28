# The stash rides along

## Summary

The sweep made a session switch refuse to run while prompts were
stashed, so nothing would be lost silently. Refusing is the wrong
half of the fix: the stash is a place to put a thought until it is
wanted, and "you may not leave this session until you spend it"
turns a convenience into a hostage. A switch rebuilds `App`, so the
stash simply has to be carried into the rebuilt one — the way a
rewind already carries its prefill and its notice.

## Requirements

- `switch_blocked` stops counting the stash; a running turn, live
  background agents and an unsent draft still block.
- Every switch path (resume, fork, rewind, the palette) carries the
  stash into the rebuilt app, alongside the existing prefill and
  notice, and the pops work there exactly as they did before.
- Ctrl-D keeps its warning: quitting really does lose the stash.

## Acceptance Criteria

- Stash two prompts, switch sessions, pop them in the new session,
  newest first, with their images.

## Milestone

13 — Guard rails

## Outcome

`AppExit::SwitchInto` grew a `stash` field and absorbed the plain
`Switch` variant, so there is one switch path and it carries
everything that must survive the rebuild: prefill, notice, stash.
Every producer (resume, fork, rewind, palette) takes the stash on
the way out; the rebuilt app is handed it before the first frame.
`switch_blocked` no longer counts the stash — a running turn, live
background agents and an unsent draft still block — and its test
was inverted to pin that. Ctrl-D keeps its warning: quitting really
does lose the stash, and nothing carries it there. Test walks the
whole handoff, including an image riding with its prompt.
