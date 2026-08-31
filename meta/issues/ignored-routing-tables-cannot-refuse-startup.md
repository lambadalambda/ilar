# Ignored routing tables cannot refuse startup

## Summary

Project `[providers]` and `[models]` tables are documented and reported as ignored for security, but the project file still passes through provider/model validation before the user-scoped values are restored. An invalid ignored entry can therefore make a cloned repository refuse to start ilar.

## Requirements

- Parse project config sufficiently to report ignored user-scoped tables without applying their provider/model validation.
- Continue validating project-scoped settings and TOML structure normally.
- Preserve warnings that explain ignored routing configuration.

## Acceptance Criteria

- Tests show invalid or unsupported project provider/model entries produce warnings and do not block startup.
- The same entries in user config still fail with path-specific diagnostics.
- Project config still cannot alter provider credentials, endpoints, or custom models.

## Notes

- Source: `crates/ilar/src/config/toml.rs:479-521`, `836-840`, `995-1032`, `1041-1130`.
- Residual of the completed provider-configuration scoping work.
