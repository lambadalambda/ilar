# The remote PWA

## Summary

Part of [[remote-control-over-iroh-index]]. A static, installable web
app on GitHub Pages that scans the QR code and drives the session over
the remote wire.

## Requirements

- QR scan with the camera (BarcodeDetector where present, a JS fallback
  otherwise) and paste-a-ticket as a second way in.
- Transcript view, prompt input, approval and question prompts, cancel.
- The browser's iroh key is stored (IndexedDB) per ticket id: pairing
  pins the key, so a new key after a reload is refused for good.
- `history.replaceState` removes the ticket from the URL after reading.
- `client::hello` gets a timeout, and a refusal (close code 403) is told
  apart from a network error.
- Manifest and service worker so it installs and starts offline.
- A GitHub Actions workflow builds the WASM and deploys to Pages.

## Acceptance Criteria

- Installed on a phone, it pairs with a session on another network and
  passes the index issue's acceptance run.

## Status 2026-11-27

Built: `web/remote` (shell, pairing, service worker), the wasm client in
`crates/ilar-remote-web`, a `REMOTE` source in the shared `app.js` with
grant and question cards, `scripts/build-remote-web.sh`, and the Pages
workflow. Checked in headless Chrome at phone size against a live TUI
over n0's relay (paired in 1.8 s, a message started a turn) and with
`scripts/remote-web-checks/cards.*`. The first iPhone scan gave a blank
page: Safari fails on relay hosts with a root dot (see DEVLOG); fixed
and checked on the iOS 26.5 simulator, a prompt from it ran a real
turn. Confirmed on the user's iPhone 2026-11-27 (Lockdown Mode needs a
per-site exception, which the page explains). Left over: a real phone on another
network; iOS has no in-app scanner (the system camera opens the link);
the app shares `lambadalambda.github.io`'s storage with other Pages
sites of that account. (Fixed 2026-11-27: the app moved to its own origin,
<https://remote.ilar.app/>.)
