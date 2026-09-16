# A credential flag is not a name

## Summary

`crate::redact` hides a secret when something *names* it — a
sensitive key, a header, an auth scheme, a URL's userinfo. A
credential passed positionally after a flag has no name, so it
survives:

```
curl -u bob:hunter2 https://api.example.com
```

`-u` is not a sensitive key and `bob:hunter2` is not a URL. The same
goes for `--user`, and for `psql -W` style prompts where the value
follows on the next token. Pre-existing on both surfaces; the merge
into one module just gave it one place to be fixed.

The catch is that `-u` means credentials in `curl` and `wget` and
something else entirely in `sort`, `chmod` and `docker run` — where
hiding the next token would blank a filename or a user. So the rule
has to read the command's first token, which nothing in the token
pass does today.

## Requirements

- A table of (program, flag) pairs whose value is a credential, at
  least `curl`/`wget` with `-u`/`--user`.
- The value is hidden and collected, like every other secret.
- `sort -u file.txt`, `chmod -u …` and `docker run -u root` are
  untouched.

## Notes

Found by the review of "one redaction engine" (2026-09-16), along
with two blockers that were fixed there.

Size: S. Source: review follow-up 2026-09-16.
