# Chat replies: small frictions

## Summary

Omnibus; tick items off here.

- Command names are case-sensitive (commands.rs:46-68): a phone
  that capitalises the first word gets "No command /Help." plus the
  whole help, while `/grant`'s span is lowercased for exactly that
  reason. Lowercase the name before matching.
- Review replies contradict themselves: `Plan::apply` lines like
  "user: not written — …" (review.rs:855-892) are wrapped as "💾
  remembered: …" (gateway.rs:563, 967); `/reject ab12` answers
  "Dropped 1."; `/approve foo` answers "Nothing pending as foo.";
  `/pending` shows "(deltachat:12)" keys nobody recognises.
- A caption on a picture is never split: text with media rides as
  the first file's caption whole (deltachat/mod.rs:427-436), so a
  long reply with a screenshot folds behind "Show full message".
- Three numbers for one bubble cap: "about 3000 characters"
  (mod.rs:300-301), "3,800 characters or 38 lines" (docs/gateway.md),
  34 lines × 100 chars (mod.rs:446-447). Docs say `<state
  dir>/gateway/…` for cron, memory and accounts where the code uses
  `gateway.home`.
- `ilar-gateway notify` text is parsed as a slash command
  (gateway.rs:1250-1262): a script's "/new" resets the last active
  chat. Skip `commands::parse` for `notify:` senders.

Size: S. Source: UX sweep 2026-09-15, gateway.
