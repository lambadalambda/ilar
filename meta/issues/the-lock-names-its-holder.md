# The lock names its holder

## Summary

The lock file is empty, so "already active in another turn (its
driver may be another ilar process)" cannot say which process —
useless against a zombie in another tmux pane, as seen live. Write
`pid\nstart-time` into the lock on acquire and quote it in the
refusal (and in delete()'s, which today borrows the wrong wording).

Size: S-M. Source: sweep 2026-08-29, store.
