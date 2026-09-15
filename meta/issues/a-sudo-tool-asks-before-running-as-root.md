# A sudo tool asks before running as root

## Summary

The model sometimes needs root for one command: a package install, a
service restart, a port under 1024. Today `sudo` under the bash tool
fails fast by design (no controlling terminal for the password prompt)
and there is no way to say yes to one command. A `sudo` tool runs one
command as root after the person has read that exact command and
agreed, through the same grant prompt the secret store uses.

## Requirements

- Off by default; `agent.sudo = true` installs the tool. The gateway's
  `safe_mode` denies it like `bash`.
- `sudo {command, reason}`: the person is asked "sudo wants root
  (reason) to run: <command>" with once, this session, always, deny.
  Nobody to ask means refused, with the CLI line that grants standing
  approval (`ilar secret grant root --tool sudo`).
- Standing approval is kept in the secret store as the pseudo-secret
  `root`; `ilar secret list` shows it and `ilar secret revoke root`
  drops it. `root` cannot be stored as a value.
- Authentication: `sudo -n` when the system needs no password; with a
  stored `SUDO_PASSWORD` the tool feeds it on stdin (`sudo -S`), never
  on the command line. The command approval covers that use; no second
  prompt. A refusal for want of a password says how to store one.
- Output goes through the same capture, spill, tail and redaction as
  bash; the password never appears in a result.

## Acceptance Criteria

- With approval standing, the tool runs the command through the sudo
  binary with the expected arguments and, when a password is stored,
  feeds it on stdin.
- Without approval and without anyone to ask, nothing runs and the
  error names the CLI line.
- A deny from the prompt runs nothing.
- Config, policy and docs cover the flag and the pseudo-secret.

## Notes

- `requiretty` in sudoers defeats it; the error is sudo's own.
- The tool is the ask, not a sandbox: once approved, the command runs
  as root and can do anything.
