# New files should honor the umask

## Summary

`atomic_file`'s `Mode::Preserve` on a nonexistent destination
computes `final_mode = None` (atomic_file.rs:91-96), so no chmod
happens and the published file keeps `create_temp_at`'s hardcoded
`0o600` (atomic_file.rs:172). The write tool uses `Preserve`, so
every *new* file it creates is unreadable by group/other regardless
of umask — surprising for source files.

## Requirements

- When the destination does not exist, `Preserve` applies
  `0o666 & !umask` (normal creation semantics) instead of the temp
  file's 0600.

## Acceptance Criteria

- A test: writing a new file under umask 022 yields mode 0644;
  overwriting keeps the existing mode.

## Milestone

12 — Health sweep
