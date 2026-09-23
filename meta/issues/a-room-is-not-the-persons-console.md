# A room is not the person's console

## Summary

Groups arrived in `e9019d2`. The chat commands did not learn about
them, and two of them act on everything with no argument.

## Requirements

- A bare `/approve` or `/reject` acts on every staged memory
  (commands.rs:119-120). On Telegram the menu entry, and the tappable
  `/approve` inside "📝 I would remember … — /approve ab12"
  (gateway.rs:1619), send the bare word. Without an argument, list
  and ask; only an explicit `all` acts on all.
- `/pending`, `/approve`, `/reject` ignore `Pending.chat`
  (review.rs:436, 469-496): a room sees and decides the person's
  private memory. Filter by chat, or refuse in rooms.
- Every command runs in a room (gateway.rs:584-590): any allowlisted
  member can `/restart`, `/model x --save`, `/unlock`, `/password`.
  Refuse secret and global commands in rooms ("send that to me in
  private", still deleting a secret-bearing message). Register a
  smaller Telegram menu with `scope: all_group_chats`
  (telegram/mod.rs:157-163); `UNLOCK_HINT` must not say "in this chat"
  in a room (commands.rs:78, grants.rs:283).
- The start/stop announcement and `notify` go to `routes.last_active`,
  which can be a group (gateway.rs:1759, 1894). Use the last private
  chat, as the weekly review does.
- `/new` in a room says "What I remember about you stays."
  (gateway.rs:1202); rooms have no memory.

## Acceptance Criteria

- Tests for each: bare approve lists, room pending is scoped, room
  refuses /restart and /unlock, announce skips a group.
- Full gate green.

## Notes

- Source: UX sweep 2026-09-23 (gateway pass). Size: M.

## Done (2026-09-23)

`cfdc09d` and `727512b`: bare `/approve`/`/reject` list and ask; a room
is refused `/pending`, `/approve`, `/reject`, `/restart`, `/unlock`,
`/password`, `/model … --save` and `/grant always`, and Telegram
offers it a smaller menu (a unit test keeps the two lists in step); a
room's sudo password ask is refused on the spot; announcements and
`notify` go to the last private chat; `/new` in a room promises no
memory.
