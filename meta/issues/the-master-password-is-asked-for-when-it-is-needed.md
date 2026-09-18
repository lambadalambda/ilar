# The master password is asked for when it is needed

## Summary

A sealed secret store is opened once per process, on the plain terminal,
before anything else runs (main.rs:118, `unlock_secrets`). Both drivers
that have a terminal call it: the TUI before it takes the screen, and
`ilar exec` before it runs its one turn (main.rs:1319).

Two things are wrong with that.

`ilar exec` should not ask at all. It is the headless driver — it builds
its runtime with `questions: false, grants: false` precisely because
nobody is there to answer — and then stops on a password prompt before
the first token. A script that pipes a prompt into `ilar exec` and a
cron line that runs one both hang on a prompt nothing will type into.
Whatever it gets from the store it gets from standing grants, which need
no password to have been *asked for*, only the store to be readable.

And in the TUI the prompt comes far too early. It is the first thing a
person sees, every run, whether or not the session ever touches a
secret — most do not. The password belongs at the moment something
actually needs it, which is where every other secret question in ilar
already is: the grant prompt and sudo's password prompt both arrive on
the tool call that wants them, named with the tool that asked.

## Requirements

- `ilar exec` never asks for the master password. A sealed store stays
  locked there, and the refusal a locked store causes says so rather
  than pointing at a start prompt exec does not have.
- In the TUI the master password is asked for at the first use that
  needs it — a `secrets` listing, a `bash`/`service` call naming a
  stored secret, a `sudo` — through the same channel and the same kind
  of modal the other two secret questions use, naming the tool that is
  waiting.
- A wrong password is asked again, as at the start prompt; a cancel
  leaves the store locked and fails that one call, the way a denied
  grant fails one call.
- The unlock hint each driver puts in its refusals matches what that
  driver can actually do.
- A provider key kept in a sealed store still reaches the TUI: the
  configuration is read before any tool runs, so the start prompt has to
  survive for the one case where deferring it would strand the session —
  the configured model cannot be reached without the store.

## Notes

The gateway already does this the lazy way and is the precedent: it
never had a start prompt, and `/unlock <master password>` opens the
store mid-session from the chat.

The provider-key limitation exec inherits is the one the gateway
already documents (docs/secrets.md: "keep provider keys in `ilar.toml`
on that box"). Same sentence, one more driver.

Raised by the user, 2026-09-18: "we currently ask for the secrets store
in ilar exec, that really shouldn't happen."

Size: M. Source: user report 2026-09-18.
