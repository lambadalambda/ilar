# Sweep after the write path

## Summary

Three-reviewer adversarial sweep over the post-rc3 work (advisory
reads, the serve workspace layout, the serve write path, the
pre-rc3 tools batch), 2026-08-28. Findings below are the ones that
survived tracing; each names its own fix. Dissolved candidates are
in the review transcripts, not here.

## Requirements

### Security and correctness — serve

- **DNS-rebinding RCE.** Loopback is tokenless by design and there
  is no `Host`/`Origin` validation, so a hostile page can resolve
  its own name to 127.0.0.1 and POST `/api/sessions` with a shell
  prompt. Read-only serve made this exfiltration; the write path
  makes it execution. Reject requests whose `Host` is not the bound
  address or a loopback name, before the token check.
- **Resume runs in the server's cwd** (`drive.rs:247`), not the
  session's, so tools, subagents and project instructions come
  from the wrong tree — while the page says otherwise. Use the
  session's recorded cwd.
- **A failed turn is invisible**: `run_turn`'s `Err` goes to the
  server's stderr only, after the 200. Surface it to the page.
- **A steer after abort is accepted and dropped**: abort cancels
  the token but leaves the registry entry, so the next message
  reports `steering` into a loop that has already returned.
- **In-process contention reports "another process"**: two sends
  racing the same idle session hit the writer lock and get the
  watching-only 409.
- **SSE resume prefers `?from=` over `Last-Event-ID`**, so an
  EventSource reconnect replays everything since the tab opened.
- Cheap hardening while there: registry cleanup in a drop guard,
  poison-tolerant registry lock, a cap on concurrently driven
  sessions (or an honest doc note).

### Frontend

- A 409 disables the composer with no path back.
- An in-flight send/abort resolves against whatever session is
  selected when it returns (wipes a draft, misattributes fate).
- A closed EventSource shows "reconnecting…" forever.
- Pinned tokens are not percent-decoded server-side and not
  encoded into the printed fragment.
- "Load earlier" remounts every row (keys carry the sliced index).
- `rewind` has no upper bound; a short client extends the array
  with holes.
- Transient children-poll failure blanks the subagents panel.

### Core and tools

- `tests/workspace.rs` still asserts read-blocks-write: the suite
  is red.
- `inherited_lease` is computed before the capacity demotion can
  flip `background`.
- The waiting notice covers the plain-permit branch only, so
  `edit`/`write` and mutable tasks wait silently; and a tail is
  only visible on an expanded row.
- JSON shape: replaces a stdout that fit on its own; advises `jq`
  on a file that also holds stderr; loses the preview entirely
  when the spill write fails; ignores `preview_bytes`.
- grep spill claims "every match" over already-capped output.
- Stash is destroyed by a session switch and by Ctrl-D; a pop
  leaves attached images on the wrong message.
- Gutter skip fires on inline code and misses rows behind a
  speaker label or a subagent tree bar.
- Ctrl-L is unreachable while a modal is open.
- `service` clears its process group when the direct child exits,
  so a daemonizing service outlives the session.

## Acceptance Criteria

- Each fix lands with a test that fails without it. The suite is
  green. Findings deliberately not fixed are listed in the outcome
  with the reason.

## Milestone

13 — Guard rails
