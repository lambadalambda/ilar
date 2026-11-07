# Agents can read scoped source transcripts

## Summary

The model's history tool reads only its current session. The TUI can search other sessions for the person, but a memory validator has no equivalent scoped interface for reading a note's original evidence.

## Requirements

- Record the access policy and tool interface before code. Do not make arbitrary session IDs readable merely because the caller knows them.
- Let an authorized memory-validation caller search and read evidence within the same memory ownership scope: the project, including its worktrees, or the gateway's allowed private history.
- Define child-session access explicitly. A root may need a relevant child's evidence; delegated agents must not gain all parent memory or transcripts by default.
- Keep group and unrelated-project evidence outside personal/project validation scope. Enforce access checks on both search and reads, without revealing withheld snippets or metadata.
- Read original events after compaction, with bounded results and continuation. Missing or retired sources must produce an explicit unavailable result.
- Reuse the existing session archive and [bounded history reads](history-can-show-the-evidence-it-finds.md), rather than scanning raw log files through unrestricted shell tools.

## Acceptance Criteria

- Tests allow evidence from an earlier session in the same project and another worktree of that project.
- Tests reject unrelated-project and unauthorized gateway/group sources on both search and direct reads.
- Tests cover the chosen child-session policy and prevent ID-based access bypasses.
- Source references from [memory notes](memory-notes-keep-their-evidence.md) can retrieve original events after compaction, within the output limit.

## Notes

- Part of [the memory follow-ups](memory-follow-ups-from-the-component-review.md).
- Current restriction: `crates/ilar/src/tools/history.rs:190-203`. TUI search: `crates/ilar/src/recall.rs:436-445`.
- This issue does not grant general cross-session access to every agent or change memory defaults.
