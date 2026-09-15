# Sessions list fast and true

## Summary

Measured on the Mac on 2026-09-15: 5,593 entries in the sessions
directory, of which 2,421 are session files, 2,148 of those subagent
children, 2,414 stale `.lock` files (only `delete` removes one), and
751 replay sidecars. 104 of the 273 root sessions have no user message
and no topic: the file is created at launch (runtime.rs, `create_root_
session`) before anything is typed, so every `ilar` opened and closed
leaves a "(no messages yet)" row for ever. `store.list()` opens every
session file, children included, reads a 256 KiB head plus a tail for
the topic, and is called on `/sessions`, `--continue`, `latest_in`, and
twice per content search. The content search then loads every
session's full entries (2.1 GB here). And the empty-query listing in
main.rs (`spawn_session_scan`) takes `MAX_SEARCH_ROWS` (200) newest
sessions *before* the modal partitions this directory's rows to the
top, so once 200 sessions elsewhere are newer, this directory's last
session is not shown at all.

## Requirements

- Listing cost is proportional to what changed: a summary cache in the
  sessions directory keyed by file (id → mtime, length, summary,
  is-child), reread only for files whose stamp moved; children are
  skipped without opening. One cache read per listing, one write when
  it changed. The `.lock` files are removed when the writer lease ends,
  and stale ones swept with the live scratches at startup.
- No empty sessions left behind: a root session with no user message
  is removed when its runtime ends (TUI quit, exec end) unless it has
  children or an outbox entry; the startup sweep removes such files
  older than a day. Sessions that still have no title show last, never
  among this directory's rows.
- This directory's sessions lead the list before any cap: the cap is
  per group (newest N here, then newest N elsewhere), and `--continue`
  and the picker agree on what "here" is (canonical launch cwd, as
  recorded in the Meta event; a session with none is never here).
- The content search streams newest-first, reads each file's raw bytes
  for a case-insensitive substring before parsing it as JSON, lists
  once (not twice), and stays cancellable; children are not searched.
- A test for each: cache hit vs reread, lock removal, empty-session
  removal and the child/outbox exceptions, here-first-before-cap,
  search skipping non-matching files without parsing.

Size: M. Source: user report 2026-09-15.
