# Default and maximum context windows

## Summary

Provider-advertised maximum context does not always match the conservative
working context used by coding-plan clients.

## Requirements

- Store a default working context separately from the models.dev maximum.
- Use 272,000 tokens by default for GPT-5.6 Sol, Terra, and Luna.
- Retain their models.dev 1,050,000-token value as the maximum.
- Use the default working context for telemetry and compaction.
- Leave room for a later configuration option bounded by the maximum.

## Acceptance Criteria

- Sol, Terra, and Luna report a 272,000-token default and 1,050,000-token max.
- Runtime context telemetry and automatic compaction use the default limit.
- Other catalog models retain their existing behavior.

## Notes

- Codex source: https://github.com/openai/codex/pull/34009
- Maximum source: https://models.dev/api.json
