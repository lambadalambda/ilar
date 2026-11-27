# Several devices follow one session

## Summary

`/remote` pairs one device per session today; a second is refused. A
desktop and a phone (or two phones) should be able to follow and drive
the same session, as the TUI and one phone already do.

## Requirements

- New devices pair with the code while the QR window is on screen (a
  copied or photographed link adds nobody after Esc); paired devices
  reconnect at any time. `/remote stop` forgets them all.
- A device's newer connection replaces only its own older one.
- Each connected device gets the feed; a reply to a command (a notice,
  an image) goes to the device that sent it.
- The QR window lists the connected devices; the status marker shows
  how many.

## Acceptance Criteria

- Two devices paired with one session both see a turn stream, and a
  prompt from either starts or steers it.
- A device that scans after the window closed is refused.

## Status

Built and checked 2026-11-27 against a live TUI: the iOS simulator and
a desktop Chrome paired with one session, a prompt from the desktop
streamed on both, and a third device was refused after Esc. A device
that vanishes without closing counts as connected until the QUIC idle
timeout.
