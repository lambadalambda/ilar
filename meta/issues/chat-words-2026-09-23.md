# Chat words, 2026-09-23

## Summary

Smaller wording and coverage gaps in the gateway. Omnibus; tick off.

## Requirements

- Provider failures read "That turn failed: HTTP 429 …{json}" with no
  next step; `EMPTY_REPLY` has one. A plain first line for 429/5xx/401
  plus "send it again or /model", clipped cause under it.
- Job failures pass `error.to_string()` (gateway.rs:1473) and never
  mention the retry a minute later.
- Stickers, locations, contacts, polls get no reply and no ack
  (telegram/mod.rs:262); pass a short note to the model. Edits are
  never received (`allowed_updates`, :627) — say so in the docs.
- An attachment over the Bot API's 20 MB reads "could not be fetched"
  (:255); name the limit.
- A mention replying to someone reaches the model without the quoted
  text (:235-238); include `(replying to Alice: "…")`. docs/gateway.md
  says privacy mode off lets it "see the conversation" — it drops
  unaddressed messages; fix the sentence.
- Staged memories can show a raw note id ("note forget: 01J…",
  review.rs:270, 349); show the title.
- No chat command takes back a grant; grants.rs:110 names a CLI
  command, and "until this chat is restarted" (:107, :299) is unclear.
- Telegram splits at 36 lines, not at 4096 chars (PIECE_LINES,
  telegram/mod.rs:79); forum topics ignore `message_thread_id`.
- `/cron` one-shot line mixes RFC 3339 and "… UTC" (gateway.rs:929).
- Menu `("deny", "refuse it")` has no antecedent (commands.rs:263);
  `/restart` promises a service restart in the foreground too.
- `ilar-gateway invite` with only Telegram says "is the gateway
  running?" (main.rs:90-97).
- Delta Chat groups answer every message; the docs do not say so.

## Acceptance Criteria

- Each item fixed or struck with a reason here.

## Notes

- Source: UX sweep 2026-09-23 (gateway, docs passes). Size: M, many.

## Done (2026-09-23)

`4539016`: a failed turn leads with a plain line for a rate limit
(status 429), refused credentials (401/403) or provider trouble (5xx,
overloaded), read from the status the transport writes rather than
any digits in the chain, and ends with what to do; a failed job says
whether it runs again. Telegram: stickers, places, locations,
contacts, polls and dice reach the model as a line; a file over the
Bot API's 20 MB says so; a reply to someone else carries
`(replying to Name: "…")`, naming a channel by its title; replies
split at 4000 UTF-16 units, never into an empty piece. Staged memory
names notes by title. Grant spans say "until /new or the next gateway
restart"; `/deny`, `/restart` and `/cron` wording; `ilar-gateway
invite` without Delta Chat says what invites are for; docs say what a
group sees and that Delta Chat groups answer everything.

Struck: a `/revoke` chat command (the CLI line stays the way back);
forum topics (`message_thread_id`) — replies still land in General.
