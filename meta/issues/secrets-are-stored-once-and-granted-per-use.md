# Secrets are stored once and granted per use

## Summary

A person wants to hand ilar an API key or a token without it landing
in the transcript, in the model's context or in every shell the model
opens. Today provider keys sit in ilar.toml or in the environment, the
bash tool inherits the whole environment, and nothing asks before a
key is used. A store keeps secrets by name; the model names them, the
person grants each use, and the value reaches the command as an
environment variable and nothing else.

## Requirements

- A store at `<state dir>/secrets.json`, mode 0600, holding name,
  description and value. `ilar secret set|list|remove|grant|revoke`
  manage it; `set` reads the value from stdin, never from argv.
- The model sees names and descriptions only: a `secrets` tool lists
  them. No tool result and no prompt ever carries a value.
- `bash` and `service start` take `secrets: [NAME, ...]`. Each named
  secret becomes an environment variable of that one command. Output
  that echoes a value is redacted at the source, before the spill file
  and the transcript, not just on display.
- Every use needs a grant. The TUI asks in a modal that shows the tool,
  the secret and the exact command: once, this session, always for this
  tool, or deny. The gateway posts the same to the chat and takes
  `/grant [session|always]` or `/deny`. `ilar exec` and any driver with
  nobody to ask fails the call and says how to grant from the CLI.
- "Always" grants persist in the store per secret and tool; `revoke`
  drops them. Session grants live with the runtime.
- Child processes no longer inherit ilar's own keys (`ILAR_*_API_KEY`,
  `ILAR_SERVE_TOKEN`) nor any variable whose value is a stored secret.
- A provider key may live in the store under its environment name
  (`ILAR_OPENAI_API_KEY`): the config loader reads it after the TOML
  field and the environment.

## Acceptance Criteria

- A stored secret named by a bash call arrives as `$NAME` in the
  command's environment and is `<secret:NAME>` in the result when the
  command prints it.
- A call naming an unknown secret fails and lists the known names.
- Without a grant and without anyone to ask, the call fails with the
  CLI hint. With a grant sender, a denied prompt fails the call, a once
  grant runs it, a session grant runs the next call without asking, an
  always grant survives a fresh runtime.
- The TUI modal and the gateway commands answer prompts end to end.
- A child shell does not see `ILAR_OPENAI_API_KEY`.
- The docs describe the store, the commands and the limits.

## Notes

- Once granted for a command the model wrote, that command can do
  anything with the value. The prompt showing the exact command is the
  safety; a session or always grant gives that up, and the default
  answer is once.
- Encryption at rest is out of scope: the gateway runs unattended and
  would need the passphrase anyway. A keychain backend can come later
  behind the same store interface.
- webfetch takes no headers, so no secret injection there; curl under
  bash covers it.
