# ilar secret is one CLI

## Summary

The `ilar secret` subcommands (crates/ilar-tui/src/secret_cli.rs)
disagree with each other and with the store:

- Exit codes: `remove NAME` and `grant NAME` on an unknown name print
  "No secret named NAME" and exit 0 (157-172); `grant --tool read`
  exits 1; `revoke NAME` on a typo prints "Nothing to revoke for
  NAME", the same as a real name with no grants. Unknown name should
  be an error for all three, listing the stored names.
- `remove root` says "No secret named root" while `list` shows a
  root row (secrets.rs:485 vs 440-446). Say "root is not stored;
  `ilar secret revoke root` drops its standing approval", or do it.
- `grant root --tool bash` is accepted and prints "bash may use root
  without asking"; nothing reads that grant but the sudo tool.
  `grant GITHUB_TOKEN --tool sudo` is accepted though sudo takes no
  `secrets:`. `revoke --tool <anything>` is not validated (179-186).
  root grants only to sudo; sudo takes only root.
- The `--tool` help says "The tool: bash or service" (25) and the
  doc comment at secrets.rs:590 likewise; `GRANTABLE_TOOLS` and
  docs/secrets.md say sudo. The clap about for `set` still says "the
  value is read from stdin (one line, or a pipe)" (11) and
  docs/interface.md:235 says "value on stdin"; since 76dd959 a
  terminal asks hidden and confirms.
- The master-password prompt "Secret store master password (Enter
  leaves it locked): " (56) is shared with the CLI, where Enter then
  bails "the store is sealed; nothing can be read or written without
  the master password" (99-103). Per-caller prompt text.
- On a sealed store `set 1bad` asks for the master password and only
  then rejects the name (98-108). Validate the name first.
- `encrypt` reads both entries before rejecting a short password
  (secrets.rs:369-371; the 4-character minimum is not in
  docs/secrets.md), and its mismatch text "the two did not match"
  differs from `set`'s "…; nothing stored".
- Empty `list`: "No secrets stored. Add one with: ilar secret set
  NAME (/path/secrets.json)" reads as if the path were an argument
  (141-144). `list` rows join with two spaces where the model's
  listing uses " — " (146-158 vs secrets.rs:712); pad or match.
- The model's unknown-name error lists `root` among "stored" and
  suggests `ilar secret set root`, which `set` refuses
  (secrets.rs:838-853). Drop the pseudo-row there.
- Store errors reach the model doubled: "bash: secrets: parsing
  secrets /path: …" (tools/mod.rs:717 wrapping secrets.rs:780).
- Top-level clap (main.rs:119-225): `--session` is "Session id to
  resume." at the root and "Session id to continue." on exec;
  `--agent` docs differ; root/exec flag docs end with a period, no
  subcommand about does; `Remove`/`Grant`/`Revoke` positionals and
  `Revoke --tool` have no doc string; `about = "personal coding
  agent"` is lowercase; `--view` conflicts only with
  `--session`/`--continue`, so `--view X --model M` ignores the
  model silently (main.rs:200-205, 1210).

Size: S, many small edits. Source: UX sweep 2026-09-15, CLI.
