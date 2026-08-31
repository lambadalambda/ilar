# Skills load one body once

## Summary

`SkillStore::list` reads and parses every complete skill body. Startup invokes that scan through `listing_prompt` and then repeats it for metadata; the model-facing skill tool repeats the synchronous full scan inside an async future and can scan again for an unknown name. Skill bodies have no input or output size limit.

## Requirements

- Separate bounded metadata discovery from on-demand body loading.
- Resolve winning skill paths once per runtime plan and load only the requested body.
- Move filesystem reads off async runtime workers.
- Bound skill files and returned bodies with a clear error.

## Acceptance Criteria

- Tests prove startup does not read each body twice and loading one skill does not read unrelated bodies.
- Oversized skill definitions fail within a documented limit.
- An unknown skill lists available metadata without a second body scan.
- Project-over-user and portable skill-format precedence remains unchanged.

## Notes

- Source: `crates/ilar/src/skill.rs:141-197`, `238-275`, `crates/ilar/src/runtime.rs:292-304`.
- Found by the current codebase review.
