# A locked store scrubs nothing and says nothing

## Summary

Every command ilar runs is scrubbed of every value the store holds,
not only the ones that command asked for — a `cat` of a config file or
a stray `env` must not reach the transcript, the spill file or the live
tail in the clear. The whole thing hangs off `Secrets::all()`, and on a
sealed store nobody has opened, `all()` is:

```rust
let mut all = self.store.all().unwrap_or_default();
```

Empty. So `redaction_set` has no needles and `shielded_env` removes
only what it recognises by *name* (`ILAR_*_API_KEY`, `ILAR_SERVE_TOKEN`)
— the "any variable whose value is a stored secret" half is gone.
Nothing anywhere says so: the tool result looks exactly like a scrubbed
one.

This is not new, but it is newly likely. Before, a sealed store was
opened at start or deliberately left shut for the session. Now the
password is asked for by the first call that *needs* the store — and
`bash` without a `secrets` argument never needs it (`grant_secrets`
returns early on an empty name list, tools/mod.rs:885). So the ordinary
case is now a session that runs commands against a locked store,
unscrubbed, and may unlock later on for something unrelated.

Some of this is unavoidable: values nobody can read cannot be
redacted. What is avoidable is doing it quietly.

## Requirements

- A command whose output was not scrubbed because the store is sealed
  says so — in the tool result, where the model and the person can both
  see it, not only on the notice row.
- Decide whether that is enough or whether a session with a sealed
  store should be offered the prompt before the first command runs
  rather than the first *secret* — and say which in docs/secrets.md.
- Nothing here asks for a password on a session that will never touch
  the store; that is what the change this came out of was for.

## Notes

`shielded_env`'s name-based half still works while locked, so ilar's
own keys never leak into a child either way. The gap is the
value-matched half and the output scrub.

Depends on nothing; interacts with
[the-master-password-is-asked-for-when-it-is-needed] (archived), whose
trade-off it is the other side of.

Found by review of the lazy-unlock change, 2026-09-18.

Size: S for the notice, M with the decision.
Source: review 2026-09-18.
