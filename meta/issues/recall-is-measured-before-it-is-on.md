# Recall is measured before it is on

## Summary

The October labels (33 recalls, 65 notes) were not kept. Build a new
set from the session logs: real root prompts with the note store as it
stood, labelled by a strong model that reads the full note bodies and
what the session then did, with a sample checked by the user.

## Requirements

- A script that collects the cases, labels them, and runs the selector
  against the labels: precision, recall, and how often it says nothing
  when nothing applies (`scripts/recall-eval.py`).
- The labelled set is kept so the measurement can be rerun — outside
  the repository (`~/.local/state/ilar/recall-eval/`), since it holds
  private prompts and notes.
- Use the note contents that actually existed at each prompt, including
  amendments and forgotten notes. Where that history cannot be recovered,
  use forward-collected snapshots or versioned synthetic fixtures; do not
  substitute current contents and call them historical.
- Match the runtime selector's inputs: message time, prior selector
  history, notes already held in context, and applicable budgets. Test
  the request construction against runtime fixtures so the script cannot
  silently become a different retrieval policy.
- Show the judge full note bodies, or explicitly mark any bounded input
  as incomplete. Keep model labels separate from human-checked labels.
- Keep a fixed validation set separate from cases used to tune the
  selector, and record the models, settings and human spot-check method.

## Acceptance Criteria

- Precision clearly above the October 23%, reported in DEVLOG, before
  `memory_recall` defaults to on. That threshold alone does not justify
  a default change.
- Regression fixtures cover a later amendment and forget without leaking
  those changes into an earlier prompt's note snapshot.
- Runtime-parity tests cover selector history, held notes, note ages and
  budget limits.
- A human-checked sample and fixed-set precision, recall, no-help behavior
  and latency are recorded, with remaining label uncertainty.

## Notes

- Part of [the memory follow-ups](memory-follow-ups-from-the-component-review.md).
- The selector script exists. The [2026-11-07 assessment](../memory-review-2026-11-07.md)
  found that it reads current note versions, clips judge bodies to 800
  characters and does not reproduce the runtime's history or held-note
  behavior. The recorded 59% precision and 33% recall are provisional.
- Whether memory improves later work is a [separate outcome evaluation](memory-is-measured-by-later-task-results.md).
