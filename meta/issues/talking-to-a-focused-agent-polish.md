# Talking to a focused agent: polish

## Summary

What the 2026-09 "Enter in a focus view messages it" work left
rough (crates/ilar-tui/src/main.rs:4189-4258, view.rs:528-536):

- The footer still says `read-only · ↑↓ scroll · Esc returns` for a
  running agent and `agent finished · Esc returns` for a finished
  one; Enter messages or resumes. The comment at main.rs:4189-4193
  is the stale source.
- Enter is offered on every row but `message_task` refuses a
  foreground task, a foreign-root row and a grandchild
  (subagent.rs:1462, 1482) after the send is already recorded: the
  root shows `→ build · fix tests: …`, then "message to … failed:
  task 7c1e… is a foreground task of the turn you are in …" — model
  prose with a UUID. Refuse before sending, in one human sentence.
- A successful send prints the tool's text in the root transcript:
  "Message queued for running task 7c1e…; it reaches that task at
  its next step … do not repeat the message" (subagent.rs:1489-1518
  via main.rs:3391-3395). Map outcomes to the TUI's own line.
- Resuming a finished agent drops its whole answer, up to 16 KiB,
  into one muted System row, markdown unrendered, though the focus
  view already shows it rendered. A one-line headline instead.
- The draft is shared with the root: text typed before opening the
  focus goes to the agent on Enter; text typed in focus becomes the
  root's draft on Esc (`open_agent_focus`, main.rs:2319-2350;
  `close_focus`, app.rs:1552). Stash on entry, restore on exit.
- `/` in focus pops the commands completion (`↑↓ · Tab/↵ complete`)
  while ↑↓ scroll the view, Tab does nothing and Enter sends the
  literal `/sessions` to the agent (view.rs:1050-1071). Hide it or
  refuse slash text there.
- Every global key is dropped silently in focus: F1, Ctrl-P, Ctrl-Q,
  Ctrl-F, Ctrl-T, Ctrl-O, Ctrl-S, Ctrl-V, F2/F3; Ctrl-D on a blank
  draft neither quits nor says why, while the input footer promises
  `Ctrl-S stash`. Route them or say "Esc leaves the view first" once.
- Ctrl-D counts the stash and undelivered results for its warning
  but not `focus_messages` in flight, which are aborted on quit
  (main.rs:3515-3526, app.rs:2210-2228). Count them.
- The send and the reply are transcript-only, never written to the
  root's log, so a restart shows no trace; docs/interface.md says
  "The root's transcript records the send". Record an event or
  say "shows".
- Help's Session section puts the focus line between `/rewind` and
  `^Y in that picker`, so "that picker" now points at the focus view
  (modals.rs:882-887); help has no line for Esc leaving a focus view.
- `agents (N)` counts ⚙ job rows and ✉ deliveries; a job's second
  line puts "job" in the agent-name slot (view.rs:905,
  sidebar.rs:163-181). Count agents only or retitle.
- docs/interface.md "Talking to a focused agent" opens with the
  background-job paragraph that belongs under "The sidebar";
  docs/agents-and-skills.md:79-82 describes the panel without jobs or
  delivering rows.

Size: M. Source: UX sweep 2026-09-15, TUI.
