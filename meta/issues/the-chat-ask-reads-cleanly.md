# The chat ask reads cleanly

## Summary

The gateway's grant flow (crates/ilar-gateway/src/grants.rs,
commands.rs, gateway.rs), wording and shape:

- The ask runs the verbatim command straight into the instructions
  (grants.rs:88-95): a multi-line command's last line is followed by
  "/grant allows it this once…" with nothing marking the boundary,
  and a command past 34 display lines is split across bubbles with
  the 🔑 header in one and the instructions in another, no "N more
  lines" marker (the TUI shows one, ilar-tui grants.rs:230-233).
  Blank lines or an indent around the command, and a cap with a
  tail, or a stated verbatim-always policy.
- A subagent's ask is not marked: `ask_text` ignores `session_id`;
  the TUI says "bash (subagent) wants X". Compare against the seat's
  session id and say so.
- `/unlock <password>` leaves the master password in the chat
  history on every device, right or wrong, with no warning and no
  cleanup (gateway.rs:609-624; the adapter can delete messages,
  deltachat/mod.rs:388). The reply and docs/secrets.md should say to
  delete the message; better, the adapter deletes the inbound one.
- The password lifetime is described as "until the gateway stops"
  (grants.rs:83-84, docs/gateway.md:149) but `held` lives in the
  seat's `Secrets`, dropped on `/new`. Say "until this chat is
  restarted", as the Session verdict does.
- `/unlock` alone replies "No command /unlock (the master password
  goes after it)." plus the whole help (commands.rs:62,
  gateway.rs:572). A usage line without the help dump.
- The 🔑 ask takes the "working…" status line down (`send` calls
  `end_status` for every outbound, gateway.rs:1109-1111) and nothing
  brings it back after `/grant`.
- Verdict punctuation: "X denied for bash. (no answer)" should be
  "X denied for bash (no answer)."; the Always verdict names
  `ilar secret revoke X`, which revokes every tool, where the docs
  show `--tool bash`.
- HELP lists `/abort` but not its `/stop` alias (commands.rs:95-103);
  docs/gateway.md:142 says `/new` starts "on the configured model"
  while the code prefers the saved default; "no model foo/bar; /model
  lists them" (driver.rs:572) is the one lowercase, unpunctuated
  reply.

Size: S, many small edits. Source: UX sweep 2026-09-15, gateway.
