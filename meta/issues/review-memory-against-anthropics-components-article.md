# Review memory against Anthropic's components article

## Summary

Compare ilar's current memory implementation with Anthropic's "The components of agent memory" article, including the terminal and gateway paths.

## Requirements

- Map transcript search, memory notes, retrieval, injected context, and background consolidation to the current code.
- Separate confirmed implementation problems from design choices and unmeasured behavior.
- Check relevant tests and recommend focused follow-ups without changing runtime behavior.

## Acceptance Criteria

- Record the assessment with source references and the article's limits.
- Report relevant test results and rank the main follow-ups.

## Notes

- Source: https://www.anthropic.com/research/components-of-agent-memory
- This is a review, not a request to implement every mechanism in the article.

## Outcome

Completed 2026-11-07. The [assessment](../memory-review-2026-11-07.md) maps the implementation, records 174 passing focused tests and an isolated lost-update diagnostic, and ranks follow-ups. Three confirmed write problems have separate open issues. No runtime behavior changed.
