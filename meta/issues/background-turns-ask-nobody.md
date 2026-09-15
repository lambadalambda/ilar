# Background turns ask nobody

## Summary

Every gateway plan runs with `grants: true` (driver.rs:728), so a
cron or heartbeat seat (`background_seat`, driver.rs:195-206,
254-265) posts "🔑 bash wants X … to run: …" to its target chat. The
person's `/grant` is looked up under the chat's own seat
(gateway.rs:530-541), whose slot is empty: "Nothing is waiting for a
grant." Ten minutes later the chat reads "X denied for bash. (no
answer)" and the cron turn is told the user denied it, which the
user never did. docs/secrets.md:68-69 promises the opposite: a
scheduled turn refuses an ungranted secret and says how to grant it.

## Requirements

- Either background seats run with `grants: false` (headless refusal
  with the `ilar secret grant` hint, as documented), or `/grant` and
  `/deny` also reach the background seats homed on the chat.
- A test that a cron turn's secret ask ends the way the docs say.

Size: S. Source: UX sweep 2026-09-15, gateway.
