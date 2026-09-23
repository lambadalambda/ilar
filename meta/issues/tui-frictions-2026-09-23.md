# TUI frictions, 2026-09-23

## Summary

What the TUI passes (code read and a live look at four sizes on
tenco) found. Omnibus; tick off.

## Requirements

Bugs:
- Session-search preview joins lines with no space and shows markdown
  raw (modals.rs ~2448, one styled line drops `\n`).
- The slash popup keeps a stale selection when the query is edited
  (`slash_selected` reset only on completion, app.rs:1046/1064,
  view.rs:1231): `/m` shows `/mcp-via-cli` first, Tab gives `/compact`.
- The help overlay is 72 wide at every size, so actions get 43 cells
  and lose their ends ("abort tur…"); the comment says 53
  (modals.rs:894, 1127).
- The empty pending manager clips with no wrap (modals.rs ~1082).
- The topic reaches the window title and title bar without control
  characters stripped (main.rs:3397, view.rs ~770, topic.rs:32).

Confusing:
- An agent's view (message it, Ctrl-G) is reachable only by mouse at
  ≥121 columns (`open_agent_focus` has one caller). `switch_blocked`
  says "Ctrl-G one", which does nothing at the main view
  (main.rs:3162). Add a keyboard path (palette "Focus agent…").
- `--view` greets with "Enter sends, Ctrl-J newline, Ctrl-P commands"
  (app.rs:799), all refused; F1 there gives the read-only refusal
  (watch.rs:79); no topic in the frame title; ctx shows `—%`.
- Below 100 columns the status line ends in an unlabelled percent
  (view.rs:283-296).
- Tree connectors vanish at 60 columns.
- Esc and Ctrl-C drop a one-line draft silently; only multi-line
  drafts go to the stash (app.rs:2793).
- Ctrl-F with an empty query says "no matches".
- The theme picker labels the default "saved" with an empty config
  and hides the theme id (modals.rs:3436).
- Ctrl-R's retry prints the same error block again with no retry mark.
- Share and export write into the checkout (`app.cwd`), overwrite a
  same-topic file, and "session shared to …" suggests an upload
  (app.rs:3289, 3321). Share is undocumented.

Polish:
- One count, three names, bash jobs counted as agents: quit warning
  "background agent" (app.rs:2859), `switch_blocked` "task(s)",
  pending manager "tasks". Count separately; `text::plural` for every
  `(s)` (view.rs:147, main.rs:3986/4510, decide.rs:279,
  schedule.rs:338).
- `ended abnormally` missing from the headline verbs
  (session_view.rs:163-169).
- A child's compaction shows the whole summary live
  (transcript.rs:1742); the main path folds it into a note.
- Key labels `^G` vs `Ctrl-G`; help's "d … goal/jobs twice" and
  Ctrl-R's "failed or aborted" drifted (modals.rs:868); palette
  shortcuts blank for Rewind/Compact/Context; F1 lacks /compact and
  Share.
- Slash popup capped at 64 columns covers the model name; search
  field prompt `>` equals the selection marker; scrollbar track looks
  like the border; model picker columns ragged and the name, not the
  id, is cut; no reading-width cap at 200 columns.
- A killed terminal leaves an empty session that `--continue` can pick.
- Exit sets the title to "" (main.rs:400); push/pop the title instead.

## Acceptance Criteria

- Each item fixed or struck with a reason here; the bugs with tests.

## Notes

- Source: UX sweep 2026-09-23 (TUI and live passes). Size: L, many S.

## Progress (2026-09-23)

Done: the search preview's line breaks, the slash popup's stale
selection, the help overlay's cut actions (wrapped now, up to 100
columns), the empty pending manager's sentence, and control characters
in the topic (core and title).
