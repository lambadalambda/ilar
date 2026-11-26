# Reasoning override for `ilar exec`

Add `--reasoning <level>` so headless runs can select a model's reasoning level without a config edit.

- Validate explicit levels against the selected model and report unsupported values.
- Save the selection in new and resumed sessions. Accept `default` to clear it.
- Keep existing behavior when the flag is absent.
