# Checkpoints, rewind, and recovery

ilar snapshots the working tree at the start of every turn when the
working directory is inside a git repository. This document describes
where those snapshots live, how to inspect them with plain git, how to
recover when a rewind was a mistake, what the snapshots deliberately do
not cover, and how to reclaim the space.

## What a checkpoint is

A checkpoint is an ordinary git commit, built through a temporary index
so that your real index, HEAD, branch, stashes, and working tree are
never touched. It captures every tracked file and every untracked file
that is not ignored — the same set `git add -A` would stage.

Each session's checkpoints form a parent chain, and one ref points at
the tip:

```
refs/ilar/checkpoints/<session-id>
```

The ref keeps every snapshot reachable, which protects them from
`git gc`, and keeps them out of your log, reflog, and branch list. The
commits carry a fixed `ilar` identity and the message
`ilar checkpoint <session-id>`, so they are recognisable anywhere git
shows them.

Two kinds of commit land on the chain:

- **Turn snapshots** — one per user turn, taken just before your
  message is recorded.
- **Safety snapshots** — taken immediately before a rewind restores
  the tree, so the state you are rewinding *away from* stays
  reachable. After a rewind, the tip of the chain is that safety
  snapshot.

The session log records which commit belongs to which turn: each turn
has a `checkpoint` event, and each rewind a `rewind` event naming both
the commit it restored (`tree_restored`) and the safety snapshot it
took (`tree_saved`).

## Inspecting the chain

List the chains in a repository, newest activity first:

```sh
git for-each-ref refs/ilar/checkpoints/ \
  --format='%(refname:short) %(committerdate:relative)'
```

Walk one session's snapshots (newest first):

```sh
git log refs/ilar/checkpoints/<session-id>
```

Because checkpoints are plain commits, **you can diff between any two
turns without rewinding anything**:

```sh
# What changed in the working tree between two turns ago and now?
git diff refs/ilar/checkpoints/<id>~2 refs/ilar/checkpoints/<id>

# What did the tree look like at a specific turn?
git show refs/ilar/checkpoints/<id>~1:path/to/file.rs
```

This works even for files that were never committed to your branches,
as long as they were not ignored.

## Recovering from a rewind

A rewind takes a safety snapshot before it touches anything, and that
snapshot becomes the tip of the chain. To undo a rewind's tree
restore:

```sh
# 1. The tip is the state just before your latest rewind.
git log -1 refs/ilar/checkpoints/<session-id>

# 2. Bring those file contents back.
git restore --source=refs/ilar/checkpoints/<session-id> --worktree -- .
```

`git restore` recreates and overwrites files from the snapshot but
does **not** delete files that exist now and did not exist then; if
the rewound turn had created files, remove them by hand. Note that
`git diff <ref>` only compares *tracked* files against the snapshot —
to spot extra or missing untracked files, compare the snapshot's file
list with the working tree:

```sh
git ls-tree -r --name-only refs/ilar/checkpoints/<session-id>
git status --short
```

The conversation side of a rewind is not undone by this — the session
log's rewind marker stands. The discarded messages are still visible
in the session's `.jsonl` file (the log is append-only; a rewind is a
marker that replay honours, never a deletion), so nothing written is
ever lost, but there is no command to splice them back. Rewinding is
cheap to redo, however: the unsent message is prefilled into the
input, so the common recovery is simply to continue from where the
rewind put you.

## What checkpoints do not cover

- **Ignored files are invisible in both directions.** `.env`,
  `target/`, `node_modules/` and anything else your ignore rules
  match are neither snapshotted nor restored — a rewind will not
  resurrect an ignored file the agent damaged, and will never
  overwrite or delete one either. Files that are *tracked* but listed
  in `.gitignore` stay covered: ignore rules only apply to untracked
  files.
- **Commits and HEAD are never moved.** Restores are files-only. If
  you (or the agent) committed between the checkpoint and the rewind,
  those commits remain; ilar warns that HEAD moved but does not touch
  it.
- **Submodules and embedded repositories** are captured as bare
  gitlink entries — their inner working trees are not snapshotted and
  cannot be restored.
- **Sparse checkouts**: files outside the sparse cone are recorded as
  deleted in the snapshot, so restoring across a sparse boundary is
  not safe.
- **Outside a git repository**, and for turns recorded before
  checkpointing existed, rewind still works on the conversation
  alone.
- **Subagent turns do not checkpoint.** The parent session's turn
  snapshot covers the shared workspace.

## Disk usage and cleanup

Snapshots live in the repository's object database and share storage
with your normal history — an unchanged file costs nothing, and a
changed file costs one blob. ilar never garbage-collects them: history
that might be rewound to should not quietly disappear.

When a session is finished for good, delete its ref and the snapshots
become unreachable, to be reclaimed by a later `git gc`:

```sh
git update-ref -d refs/ilar/checkpoints/<session-id>
```

To find abandoned chains, `git for-each-ref refs/ilar/checkpoints/`
(above) shows each chain's last activity; session files themselves
live under `${ILAR_STATE_DIR:-~/.local/state/ilar}/sessions/`, so a
ref whose session file is gone is safe to delete.
