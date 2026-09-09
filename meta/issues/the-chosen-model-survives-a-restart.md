# The chosen model survives a restart

## Summary

`/model` records the switch on the chat's session, but every reopen
passes `gateway.model` as the launch's model, which outranks the
session's own; after a restart the chat is back on the default. And
the default itself can only be changed in `ilar.toml`, not from the
chat.

## Requirements

- A resumed chat runs on the model its session records; the
  configured default applies to fresh sessions only.
- `/model <provider/model> --save` switches the chat and makes that
  model the default for new chats, kept in the home as `<home>/model`,
  above `gateway.model`. `/model --save` saves the chat's current
  model. `/model` shows the default beside the current model.

## Acceptance Criteria

- Tests: a switch is still in force after the gateway is started
  again on the same home; after `--save`, `/new` starts on it.
