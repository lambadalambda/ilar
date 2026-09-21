# A bot in a group answers when spoken to

## Summary

In a Telegram group the adapter hands on every message it sees, and
the seat answers every one of them. With BotFather's privacy mode on
that is only commands and mentions, which happens to be right; with
it off — which a group wants, so the bot can follow the thread it was
asked about — the bot would answer everything anyone said. And the
model is told nothing about who said it: a group turn arrives as bare
text, as if one person were talking.

## Requirements

- In a group, a message reaches the model only when it is addressed
  to the bot: it mentions `@botname` anywhere in the text, it is a
  reply to one of the bot's own messages, or it is a command. The
  rest is dropped without a word. `group_mention_only = false` turns
  the rule off for a group that is the bot's own.
- The mention is taken out of the text wherever it sits, so "hey
  @bot what's up" reaches the model as "hey what's up".
- A group message carries the sender's name, and the prompt the model
  gets in a room is `Name: text`, so it can tell who asked and who is
  being answered. A private chat's prompt is unchanged.
- Delta Chat groups get the sender's display name the same way. (It
  has no mentions; every group message still reaches the model there.)

## Acceptance Criteria

- Adapter tests: in a supergroup, a message without a mention is
  dropped; one with `@bot` mid-sentence arrives with the mention
  removed; a reply to the bot's message arrives; `/status@bot`
  arrives; with `group_mention_only = false` everything arrives.
- A gateway test: a room message from alice reaches the session log as
  `alice: …`; a private one does not carry the name.
- Docs say how to set the bot up for a group.

## Notes

- Source: user request, 2026-09-21. Size: S.
