# Clarify external sandbox requirement

## Summary

Make it explicit that ilar does not provide its own sandbox or permission
boundary and must be run inside external isolation.

## Requirements

- Add a prominent safety warning near the top of the README.
- Explain that ilar can access anything available to its process.
- Link to Agent Safehouse and nono as examples without implying they are the
  only supported approaches.
- Correct existing wording that can imply ilar supplies a sandbox.

## Acceptance Criteria

- Readers cannot reasonably mistake ilar for providing built-in isolation.
- External sandbox examples and generic container/VM alternatives are listed.
- Git worktree isolation is not presented as a security boundary.

## Notes

- Added a prominent warning directly below the project introduction.
- Explicitly lists shell, filesystem, credential, and out-of-repository access
  risks.
- Links Agent Safehouse and nono alongside container and VM alternatives.
- Corrected the design-principle wording to name the sandbox as external.
