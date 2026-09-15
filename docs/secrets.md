# Secrets

A place to hand ilar an API key, a token or a password without it
landing in the model's context, in the transcript, or in every shell
the model opens. Secrets are stored by name; the model sees the names
and what they are for; a command gets the value as an environment
variable once you have said yes to that use.

## Storing one

```sh
ilar secret set GITHUB_TOKEN --description "gh, read-only PAT"   # asks for the value, hidden, twice
pass show github/pat | ilar secret set GITHUB_TOKEN               # or piped in
ilar secret list
ilar secret remove GITHUB_TOKEN
```

At a terminal the value is asked for like a password, hidden and
confirmed; piped, it is read from stdin as it is. It is never taken
from the command line, so it stays out of shell history and process
listings. Names are environment
variable names (`[A-Za-z_][A-Za-z0-9_]*`, up to 64 characters); values
are at least four characters. The store is `<state dir>/secrets.json`,
mode 0600, written whole under a lock. In the clear, the file is as
safe as your user account; [a master password](#a-master-password)
seals it, at the cost of typing that password once per session.

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

## sudo

With `agent.sudo = true` the model gets a `sudo` tool: one command as
root, with a reason. The ask is the same prompt, for the pseudo-secret
`root`: "sudo wants root (reason) to run: <command>", once, this
session, always or deny. Nobody to ask means refused, with the line
that grants standing approval headless:

```sh
ilar secret grant root --tool sudo
ilar secret revoke root
```

`root` shows in `ilar secret list` while a tool holds standing
approval; it cannot be stored as a value.

When sudo needs a password, the prompt has a row for it: type it there,
or leave it empty on a system with passwordless sudo. What you type is
held in memory for the session, injected on sudo's stdin (never the
command line), redacted from output like any secret, and forgotten when
ilar exits. If you would rather not type it each session, store it
under the name `SUDO_PASSWORD`; the command's approval covers its use,
there is no second prompt. A standing grant still asks once per session
when no password is known, since the prompt is where one gets typed;
an empty answer counts, so a passwordless system is asked once. With
nobody to ask (`ilar exec`, a scheduled turn) a standing grant runs on
what is known, and sudo's own error says if that was not enough. A
password typed into a prompt and refused by sudo is forgotten, and the
next ask takes a new one; a stored one stays stored, and the result
says to replace it with `ilar secret set SUDO_PASSWORD`. An empty
answer on a system that wants a password after all is dropped too, so
the next ask has the row again. In the chat the password goes last:
`/grant session hunter2`; a password sent to an ask that took none is
refused, and a misspelt span (`/grant sesion hunter2`) is refused
rather than taken as the start of the password. The chat's session
is its seat, so what it holds is forgotten when that chat is restarted
— `/new`, or a gateway restart — not only when the process exits.

Systems whose sudoers sets `requiretty` refuse a sudo with no terminal;
the error is sudo's own. The tool is the ask, not a cage: once
approved, the command runs as root. And a root command that outlives
the tool's timeout is killed only as far as sudo relays the signal,
which for SIGKILL is not at all: check with `ps` after a timeout.

## A master password

The store can be sealed under a master password:

```sh
ilar secret encrypt     # asks twice, seals the file
ilar secret decrypt     # asks once, writes it back in the clear
```

Sealed, the file holds a salt, a nonce and ciphertext: the key comes
from the password with Argon2id, the JSON is XChaCha20-Poly1305 under a
fresh nonce every write. Nothing about the secrets, not even their
names, is readable without the password.

The password is asked for once per process. The TUI and `ilar exec`
ask on the plain terminal at start, before the screen is taken over. A
wrong password is asked again, three tries in all; Enter without a
password, three typos, or no terminal to ask on at all (cron, systemd,
a pipe) leaves the store locked for that session, said once on stderr.
The TUI then keeps a line on the notice row while it runs, and every
use of a secret is refused with the way out: restart and type the
password at the start prompt. `ilar secret …` asks when it needs to,
once, and a wrong password is an error there. The gateway cannot ask:
it logs that the store is locked, and `/unlock <master password>` from
a chat opens it for the life of the process. That message carries the
master password into the chat's history on every device it syncs to,
right password or wrong: the adapter takes it back out where the
channel allows it — Delta Chat only deletes the bot's own messages for
everyone, so a `/unlock` goes at least from the gateway's database —
and the reply says whether there is still one for you to delete. The
gateway reads provider keys at start, when a sealed store is still
locked, so a provider key kept there is never seen by it: on a box that
runs the gateway keep provider keys in `ilar.toml`, and weigh whether
sealing buys anything there at all, since the password has to be typed
after every restart.

A second process that reseals the store under another password locks
this one out: the password it holds no longer opens the file, so it
drops it, says the store was resealed, and asks again the next time it
can.

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
