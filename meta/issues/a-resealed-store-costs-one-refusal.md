# A resealed store costs one refusal

## Summary

When a second process reseals the store under another password, the
one this process holds stops opening the file. That is handled: the
read fails with `Resealed`, the useless master is dropped, and the
store reads as locked from then on (secrets.rs, `open_disk`).

But the call that discovers it is not the call that gets a prompt.
`unlock_if_locked` asks "is the store locked?" first, and a stale
master still sitting in `MASTERS` makes that a no — `is_locked()` is
`is_sealed() && !has_master()`, and the entry is only dropped by the
read that fails. So the sequence is:

1. call one: not locked (stale master) → no prompt → `load()` →
   `Resealed` → master dropped → the call fails.
2. call two: locked → prompt → opens → works.

One refusal, always, for something the session could have asked about
and recovered from on the spot. The model sees a failure that says the
store "was sealed again since this session opened it" and has no reason
to expect a retry to behave differently.

## Requirements

- A call that meets a resealed store asks for the new password and
  carries on, rather than failing and leaving the asking to whatever
  comes next.
- A driver with nobody to ask still fails the way it does now: nothing
  here should turn into a retry loop.

## Notes

The shape is one retry of the `unlock_if_locked` → read pair when the
read comes back `Resealed` specifically — not on `Locked`, which
`unlock_if_locked` has already had its turn at.

Worth checking whether `listing()` and `approve_root` want the same
treatment or whether `resolve` is the only path that matters.

Found by review of the lazy-unlock change, 2026-09-18.

Size: S. Source: review 2026-09-18.
