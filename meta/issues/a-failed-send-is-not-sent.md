# A failed send is not "sent"

## Summary

The message tool returns "sent to {key}" the moment the message is
queued (message.rs:755-759); the Delta Chat call happens later in
the dispatcher and on error only logs "{key}: send failed: …"
(gateway.rs:1116-1118). The same holds for every reply through
`deliver`. With the rpc briefly down or a refused file, the reply
vanishes: the model believes it answered, the chat is silent, no
retry (only the start announcement retries, gateway.rs:1143-1167).

## Requirements

- A failed send retries once the channel is back, or is reported to
  the chat and to the model.

Size: S. Source: UX sweep 2026-09-15, gateway.
