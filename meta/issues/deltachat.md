# DeltaChat

## Summary

The first real channel. picoclaw reaches Delta Chat through a Python
bridge (`scripts/deltachat_bridge.py`, 1,135 lines) speaking WebSocket
to the Go side; the bridge itself uses about twenty RPC methods and
four events of `deltachat-rpc-server`, which ships as a prebuilt
binary (`pip install deltachat-rpc-server`, 2.59 as of 2026-09-08)
and speaks JSON-RPC 2.0 over stdio. The gateway spawns that binary
and talks to it directly: no bridge, no Python, no WebSocket.

## Requirements

- A JSON-RPC-over-stdio client for the server: request ids, responses,
  and the event stream (`get_next_event`), with reconnect on process
  death. Only the methods the adapter needs.
- Account setup: from a `DCACCOUNT:` QR (chatmail) or address and
  password; display name; accounts dir under the state dir.
- Inbound: `IncomingMsg` → bus message with sender address, chat id,
  text, attachments as paths; the account's own messages and reactions
  ignored (loop safety). Groups flagged as such.
- Outbound: text, and files by path; typing via draft while a turn
  runs; an optional ack reaction on receipt.
- Contacts: `allow_from` addresses; unknown senders per the policy
  issue. First contact request from an allowed address is accepted.

## Acceptance Criteria

- Against a real chatmail account: a message from an allowed sender
  gets an answer, an image is seen, a file is sent back, a message
  from an unknown sender gets nothing; the server process dying is
  survived.
