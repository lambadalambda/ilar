# Redaction covers what tools echo

## Summary

Residue found during the redaction sweep and left out of
service-commands-are-redacted-too, which fixed the *argument*
displays:

- A tool that echoes its command back in its result body — the
  `service` tool's confirmation text does — republishes verbatim
  what the argument display just redacted. Results are not
  redacted anywhere.
- URLs with embedded credentials (`https://user:token@host/…`)
  pass every current predicate: they are not command-shaped keys
  and not sensitive key names, so they land in transcripts,
  session logs and the serve JSON intact, in arguments and results
  alike.

## Requirements

- Result text passes through the same display-side redaction the
  arguments do, at minimum for the shell-command echo cases.
- A URL-credential pattern joins the redaction predicates —
  `scheme://user:secret@` is recognizable without false-positiving
  on plain URLs.
- Redaction stays display-only: provider requests and persisted
  logs keep raw values, exactly like the existing scheme.

## Milestone

13 — Guard rails
