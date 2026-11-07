# Import Claude Code's memories

## Summary

In a folder that Claude Code worked in, Claude Code knows more than
ilar: it has `~/.claude/projects/<slug>/memory/*.md` from earlier
sessions. ilar starts empty there. An import command copies those
notes into the project's ilar store, so recall can use them at once.

## Requirements

- `ilar memory import-claude` reads the Claude Code memory directory
  for the launch directory and writes one ilar note per memory file.
- The directory: `$CLAUDE_CONFIG_DIR` or `~/.claude`, then
  `projects/<slug>/memory`, where the slug is the path with every
  character outside `[A-Za-z0-9]` changed to `-`. Claude Code shares
  one directory across worktrees, so the main checkout's path is tried
  first, then the launch directory. `--from <dir>` overrides.
- Mapping: the title from the `MEMORY.md` pointer line, else the
  `name` made readable; the summary from `description`; the body as it
  is, plus a line with the Claude name so `[[links]]` stay findable;
  the kind from `metadata.type` (user and feedback → preference,
  project → decision, reference → solution); the date from
  `metadata.modified`, else the file's mtime. Headlines over the
  limits are clipped with `fit_headline`.
- A second import adds nothing that is already there, also when the
  note was amended or forgotten since: the id carries a hash of the
  Claude name.
- `MEMORY.md` itself is not a note. A file without frontmatter is
  imported with its file name as the name.
- `--dry-run` lists what would be added.

## Acceptance Criteria

- Unit tests: slug, parsing (frontmatter, no frontmatter, long
  description), kind mapping, the directory candidates for a worktree.
- Integration test: an import into a temp store adds the notes, and a
  second import adds none, also after a forget.
- A live import of this repository's Claude memories.

## Notes

- Claude Code paths longer than 200 characters get a hash suffix that
  this does not compute; `--from` covers them.
