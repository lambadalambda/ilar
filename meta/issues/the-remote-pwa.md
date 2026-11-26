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
