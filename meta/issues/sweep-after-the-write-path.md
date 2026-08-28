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

## Outcome

Three reviewers found it, four fixers closed it, eight commits.
Everything above landed with tests except where noted.

The one that mattered most was not on anyone's list before the
review: `ilar serve` was remotely exploitable. Loopback is
tokenless because a local process could already run ilar — but a
browser is a local process a *remote page* steers, and nothing
checked which name a request arrived under, so DNS rebinding turned
`POST /api/sessions` into arbitrary execution. Requests now have to
name this server (IP literal, which cannot be rebound, or
localhost, on the bound port), Origin held to the same rule,
missing Host tolerated only on a loopback bind, checked outermost
because a loopback bind has no token check to piggyback on. Cost,
documented: a non-loopback bind reached by hostname is now 403 —
use the IP `ilar serve` prints, or a tunnel.

Two findings changed shape under tracing, and the corrections are
the useful part. (1) A failed turn *is* persisted —
`persist_failed_step` writes a Diagnostic block — but `view.rs`
drops Diagnostic on the wire, so the page never saw it; the fix
added a broadcast error frame rather than quietly widening the
projection contract. Un-dropping Diagnostic would also put the text
in the transcript, and is left as a decision. (2) The
`inherited_lease` ordering bug is real but untestable today: the
demotion only fires for defaulted-background tasks, which are
exactly the read-only agents, for whom both branches are
observationally identical. Fixed as an invariant, honestly unpinned.

Cross-agent integration bug caught in review of the reviewers: the
new non-terminal `scope:"turn"` error frame would have been treated
as terminal by the client the other agent had just rewritten,
detaching the stream after every failed turn. Fixed here, with the
banner clearing when the next turn starts.

Not fixed, deliberately: no cap on concurrently driven sessions
(stated in docs instead); the posix_spawn→fork regression from
setsid and the pid-reuse window on bash's success path
(pre-existing, both reported); palette entries for Ctrl-S/Ctrl-L;
Esc leaves history recall running the way Ctrl-S used to. Residual
trade taken knowingly: service keeps its process-group id after the
shell exits, so a session-end kill has a longer pid-reuse window
than bash's — the alternative was letting daemonized services
outlive the session, which is worse.
