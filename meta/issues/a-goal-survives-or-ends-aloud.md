# A goal survives or ends aloud

## Summary

`/rewind` refuses while a goal is active ("a goal is active — /goal
abort before rewinding away its context", main.rs:3088-3092).
`/fork`, `^Y` in the picker and resuming another session have no
guard (main.rs:3104-3137, 3811-3878, 3998-4058); `App::new()` and
`AppExit::SwitchInto` carry no goal. `/goal X`, one round, `/fork`:
the goal panel and the `· goal 1/25` badge are gone, no transcript
line.

## Requirements

- The same guard, or carry the goal, or a "goal ended" line.

Size: S. Source: UX sweep 2026-09-15, session lifecycle.
