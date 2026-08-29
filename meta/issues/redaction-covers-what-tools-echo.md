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

## Outcome

`redact_command` grew a collecting variant that records every
value it hides; `redact_tool_result` replaces those secrets
(longest-first, bounded) in the displayed result copy before
bounding, and a URL-credential pass redacts `scheme://user:secret@`
in arguments and results — with the authority scan stopping at
every character an authority cannot contain, after review caught
the first version inventing credentials inside minified JSON.
Applied at every display: the live ToolFinished publish, the
restored TUI row (call inputs tracked through the replay walk),
the web transcript projection (inputs harvested from the view;
SSE frames keep a running map per stream), and the full-text
/results route. Persisted events and provider requests stay raw
by design, pinned on both sides.

Known residue, accepted: the live output tail streams raw while a
tool runs (hidden at finish); a bare secret under four characters
or beyond the sixteen-secret cap survives; a result whose call
was compacted away falls back to the URL-only pass.
