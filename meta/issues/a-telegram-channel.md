# A Telegram channel

## Summary

The gateway speaks one channel, Delta Chat. The comparison in
[[gateway-comparison-2026-09-21]] puts a second channel first: every
other gap in that table (buttons, reactions, streaming, voice) hangs
off having a platform that can do it, and Telegram has the largest
reach and the richest bot surface.

Telegram is reached directly over the Bot API — long polling, no
bridge, no webhook, no inbound port — which keeps the "no HTTP server"
stance and needs only an outbound HTTPS client the core already has.

## Requirements

- `[channels.telegram]` in the user's `ilar.toml`: `token`,
  `allow_from` (numeric user ids or `@usernames`), `allow_anyone`,
  `ack_reaction`. An empty allowlist refuses to start, as Delta Chat's
  does. `deny_unknown_fields`.
- Inbound: text and captions; a photo (largest size), document, voice,
  audio or video is fetched through `getFile` into `<home>/telegram/`
  and handed on as a path, the way Delta Chat hands one on. A
  `/command@botname` reads as `/command`; a leading `@botname` mention
  in a group is dropped from the text. Group chats set `is_group`.
- A stranger gets no reply, not even a refusal.
- Outbound: plain text, split under Telegram's 4096-character cap at
  line breaks; images as `sendPhoto`, everything else as
  `sendDocument`; a short text rides as the first file's caption, a
  long one goes ahead of the files.
- The status line: `sendMessage` → `editMessageText` → `deleteMessage`,
  under the gateway's existing throttle.
- `delete_message` retracts a password-bearing message where Telegram
  allows it (own messages, and others' in private chats within the API's
  window), and says `false` where it does not.
- **The command menu**: the gateway's commands are registered with
  `setMyCommands` at every start, so typing `/` offers them with their
  descriptions and the Menu button lists them. One list in
  `commands.rs` feeds both `/help` and the menu.
- **Buttons**: a grant ask carries `/grant`, `/grant session`,
  `/grant always`, `/deny` as an inline keyboard; a staged review plan
  carries `/approve <id>` and `/reject <id>`. A tap answers the
  callback query and is handled exactly as the typed command from that
  sender — the allowlist applies — and the keyboard is taken off the
  message once tapped. Channels without buttons ignore them; the text
  still names the commands.
- The model is told the channel's rules through `constraints()`.
- The Bot API client is a trait with a real HTTPS implementation and a
  fake for tests, as the Delta Chat adapter has `Rpc::over`.

## Acceptance Criteria

- A test drives the adapter against a fake Bot API: a text from an
  allowed user reaches the bus, a stranger and the bot's own messages
  do not, a group message is marked as such, a photo is fetched and
  its path handed on.
- A test asserts sends chunk under the cap, media picks the right
  method, and a caption is only used when the text is short.
- A test asserts a callback query becomes the command's `Inbound` from
  the tapping user, and that an untapped-by-anyone-else keyboard is
  removed after the tap.
- A test asserts `setMyCommands` is called at start with every command
  in `commands::HELP`.
- The gateway's own integration test for buttons: a grant ask sent
  through a `FakeChannel` carries the four buttons.
- `cargo test -p ilar-gateway` green; the full gate green.
- Live: verified against a real bot before it is called done, or the
  issue says it was not.

## Notes

- Bot API facts relied on: messages cap 4096 characters, captions
  1024; `callback_data` is at most 64 bytes; `getUpdates` long polling
  with `timeout` up to 50 s; `setMessageReaction` exists since Bot API
  7.0; a bot in a group only sees commands and mentions unless privacy
  mode is off in BotFather — which is documented, not worked around.
- Size: M. Source: user request, 2026-09-21.
