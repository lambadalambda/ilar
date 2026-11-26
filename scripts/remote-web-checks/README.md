# Remote web app checks

Browser checks for `web/remote` (the `/remote` phone app). They drive
headless Chrome over CDP from node 22+ (global `WebSocket`); set `CHROME`
to a Chrome or Chromium binary.

- `cards.html` + `cards.mjs`: the grant and question cards against a
  fake remote, no network. Copy `cards.html` into a built app
  (`scripts/build-remote-web.sh /tmp/pages/ilar`), serve `/tmp/pages`,
  then `node cards.mjs http://127.0.0.1:8091/ilar/cards.html`.
- `pwa.mjs`: the real thing at phone size. Start a TUI session, run
  `/remote`, and open the built app with the ticket from its link:
  `node pwa.mjs "http://127.0.0.1:8090/ilar/#<ticket>"`. It waits for the
  transcript, sends a message, and saves `/tmp/pwa-shot.png`.
