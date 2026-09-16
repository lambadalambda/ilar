# A withheld path survives a walk

## Summary

`ToolContext::withheld` refuses a tool call that *names* a withheld
path, spelled out or resolved against the cwd. Two ordinary spellings
still get there without naming it, and both are what a helpful model
does rather than an evasive one:

- **A walk over the parent.** `grep` or `glob` pointed at `<home>`
  names nothing withheld and walks into `<home>/memory/` on its way,
  returning the contents. Refusing every call that names an ancestor
  would refuse `read <home>/SOUL.md` too, so the fix belongs in the
  walkers: skip a withheld subtree the way they skip `.git`.
- **A shell that moves first.** `cd <home> && cat memory/USER.md` in
  one `bash` call resolves nothing the gate can see. This one is the
  kernel sandbox's, not ours — filed here so the pair is written down
  in one place.

A sibling leak, same class and different directory: a room's seat can
read the *private* seat's session logs under the state directory,
which hold whatever the private seat was told about its person. The
memory directory is not the only thing worth withholding from a room.

## Requirements

- `grep` and `glob` skip withheld subtrees during the walk, and say
  nothing about what they skipped.
- Decide whether the gateway withholds the session store from a room's
  seat as well, or whether that waits for the sandbox.
- Tests: a walk over the parent of a withheld directory returns
  nothing from inside it.

## Notes

Found by the review of "a room seat cannot name the memory directory"
(2026-09-16). The guard there is deliberately a guard rail, not a
boundary — see meta/issues/kernel-sandbox-for-tool-processes.md, which
is the only thing that closes the `bash` case.

Size: S-M. Source: review follow-up 2026-09-16.
