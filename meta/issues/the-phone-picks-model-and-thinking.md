# The phone picks model and thinking

## Summary

A menu in the phone app shows the session's model and thinking level
and changes them, as the TUI's model picker does.

## Requirements

- The wire sends the model list, the current model and the thinking
  levels it allows; the phone sends a choice.
- The TUI applies it as its own picker does, and only while it may
  (not mid-turn if the picker forbids that).

## Acceptance Criteria

- From the phone, switch the model and thinking level; the TUI status
  bar shows the change, and the next turn uses it.

## Status

Built 2026-11-27: `ToDevice::Models` / `FromDevice::Model` (ALPN
`ilar/remote/2`), the TUI applies a choice through
`adopt_model_selection` and refuses it mid-turn; the phone's menu has
Model and Thinking rows. Checked on the iOS simulator against a live
TUI: switched to Kimi K3.1 Pro at High, the TUI status showed it, and
the next turn ran on it. Open: the context limit in the menu is the
snapshot's until the next reconnect.
