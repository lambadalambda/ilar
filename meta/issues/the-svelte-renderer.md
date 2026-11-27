# The Svelte renderer

## Summary

Port `crates/ilar-tui/src/web/assets/app.js` (Preact + htm, no build) to
Svelte in `web/ui`, built by Vite into one self-contained `app.js` and
`app.css` that serve, share and the phone app embed as now. Redesign it
as a native chat app: bubbles, large touch targets, bottom sheets.

## Requirements

- Same features per source as today: serve (session list, new session,
  subagents, images, details panel), share (offline, one file), remote
  (grant and question cards, prompt, cancel).
- Nothing from a session reaches an HTML parser (no `{@html}`).
- The build output is committed; `scripts/build-web-ui.sh` rebuilds it
  and the gate checks that it is fresh.

## Acceptance Criteria

- serve, a share file and the phone app render a real session in a
  browser, checked in Chrome and the iOS simulator.
- The Preact app and its vendored modules are gone.
