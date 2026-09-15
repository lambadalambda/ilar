# Secrets

A place to hand ilar an API key, a token or a password without it
landing in the model's context, in the transcript, or in every shell
the model opens. Secrets are stored by name; the model sees the names
and what they are for; a command gets the value as an environment
variable once you have said yes to that use.

## Storing one

```sh
ilar secret set GITHUB_TOKEN --description "gh, read-only PAT"   # value on stdin
pass show github/pat | ilar secret set GITHUB_TOKEN
ilar secret list
ilar secret remove GITHUB_TOKEN
```

The value is read from stdin, never from the command line, so it stays
out of shell history and process listings. Names are environment
variable names (`[A-Za-z_][A-Za-z0-9_]*`, up to 64 characters); values
are at least four characters. The store is `<state dir>/secrets.json`,
mode 0600, written whole under a lock. Values are stored in plain text:
the file is as safe as your user account, no more. That is the
deliberate trade: ilar and its gateway run unattended, so a passphrase
would have to sit next to the file anyway.

## Using one

The model learns what is stored from the `secrets` tool, which lists
names, descriptions and standing grants and never a value. The tool is
present while the store has something in it and absent otherwise, so
an empty store costs no tool.

To use a secret the model names it in the `secrets` argument of `bash`
or `service start`:

```json
{"command": "gh pr list", "secrets": ["GITHUB_TOKEN"]}
```

Each named secret becomes an environment variable of that one command.
It is not in ilar's own environment, not in any other command's, and
not in the tool's arguments. What the command prints of it comes back
as `<secret:GITHUB_TOKEN>`: the captured output is redacted before the
spill file, the live tail, the service log or the transcript sees a
byte of it. On top of that, every tool result that leaves the executor,
whatever tool produced it, has every stored value replaced, so a `read`
of the store file or a `grep` that crosses it shows marks, not values.

Two limits are worth knowing. A value the command transforms (base64,
a hash, a substring) is not recognised. And once a command runs with
the value, it can do anything with it, including sending it somewhere:
the prompt showing the exact command is the whole safety, which is why
the default answer is once.

## Granting a use

Every use needs a grant. Where it is asked depends on the driver:

- **The TUI** pauses the turn on a prompt: the tool, the secret, its
  description, and the command verbatim. Allow once, allow for this
  session, always allow for that tool, or deny. Details in
  [the interface](interface.md#secrets).
- **The gateway** posts the same to the chat the seat belongs to.
  `/grant` allows it once, `/grant session` or `/grant always` for
  longer, `/deny` refuses; ten minutes without an answer is a no.
- **`ilar exec`**, a scheduled gateway turn, and any other driver with
  nobody to ask refuse an ungranted secret and say how to grant it.

Grants are per secret and per tool. Once covers that one call (for
`service start`, that one start; the value lives as long as the
service). Session lasts until the runtime ends, which for the gateway
is the chat's seat: a restart or `/new` clears it. Always is written to
the store and survives everything:

```sh
ilar secret grant GITHUB_TOKEN --tool bash
ilar secret revoke GITHUB_TOKEN --tool bash
ilar secret revoke GITHUB_TOKEN          # every tool asks again
```

Storing a secret again under the same name keeps its grants: a rotated
key is the same secret.

## What a child shell no longer sees

With or without a store, a command run by `bash` or `service` no
longer inherits ilar's own credentials: `ILAR_*_API_KEY` and
`ILAR_SERVE_TOKEN` are removed from its environment. With a store, any
variable whose value equals a stored value is removed too, so a token
you export in your own shell and also store under some name reaches a
command only when the model names it and you grant it. A script that
relied on reading ilar's keys from the environment will find them gone;
store what it needs and let it ask.

## Provider keys

A provider key may live in the store under its environment name:

```sh
ilar secret set ILAR_OPENAI_API_KEY
```

Configuration resolves a key from the TOML field, then the environment,
then the store, in that order. Ilar's own use of a provider key is not
a tool use and needs no grant.
