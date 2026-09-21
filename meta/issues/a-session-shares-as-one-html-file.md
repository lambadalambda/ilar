# A session shares as one HTML file

## Summary

`Export` writes Markdown. Markdown is fine for a diff review and poor
for a conversation: tool calls, diffs and subagent timelines all
flatten into prose, and nothing folds.

Write a second export: one self-contained `.html` file carrying the
whole session — data, styles and renderer inlined. It opens from
`file://` with no server, no account and no network, and the person
sends or hosts it themselves.

Prompted by opencode's share links. The difference is deliberate:
theirs uploads to a hosted service and returns a URL, ours produces a
file that has not left the machine until you send it. A hosted link
stays possible later and this is its prerequisite either way — the
rendering is the same work.

## What it reuses

`ilar serve` already has the whole renderer, and it is the right one:

- `serve/view.rs` — "Pure projection: no IO, no HTTP, no store".
  Session events → wire JSON, bulk and secrets cut with the same core
  helpers the TUI renders with. Trivially liftable out of the feature
  gate because it depends on nothing serve-shaped.
- `serve/assets/` — `app.js` (1994 lines), `app.css` (1019), and
  vendored preact/hooks/htm. All local, no CDN, no build step, and
  already `include_str!`-ed into the binary.
- Only two `fetch(` call sites in `app.js`, both behind one helper, so
  reading from inlined data instead of the API is a branch rather than
  a rewrite.

A third transcript renderer was the alternative and is refused: the two
that exist have drifted apart more than once, and this issue is not
worth a third.

## Requirements

- One file, no external references: styles, renderer, vendored modules
  and session data all inline. Opening it offline renders the session.
- The same content the TUI shows: user and assistant turns, tool calls
  with their arguments and results, diffs, thinking, and each
  delegation's child timeline.
- The whole conversation, not the window: `store.whole_events`, so a
  compacted session shares in full and a rewound turn stays withdrawn.
  (See [[an-export-stops-at-the-last-compaction]].)
- Secrets are cut by the projection, as they are for `serve`.
- `serve` keeps working exactly as it does, off by default, sharing the
  one renderer rather than a copy.
- The file says what it is: session title, model, turn count, cost.

## Acceptance Criteria

- A test asserts the written file references no `http://`, `https://`
  or absolute path — nothing to fetch.
- A test asserts a known user message, a tool call and a child
  timeline all appear in the file's data.
- A test asserts a secret value stored at capture time does not.
- The file renders in a real browser, verified rather than assumed:
  Firefox headless against a generated fixture.
- `cargo test` with and without `--features serve` both pass.

## Notes

- Requested by the user, 2026-09-21, referencing
  https://opncd.ai/share/uKFuqZy6 (a hosted SPA — the page's HTML
  carries nothing, which is what a fetch of it shows).
- **The one technical unknown**: `app.js` is an ES module importing
  the bare specifiers `preact`, `preact/hooks` and `htm`, and
  `hooks.module.js` imports `preact` itself. An inline
  `<script type="module">` cannot be imported by specifier, and ES
  modules from `file://` hit opaque-origin rules. The intended shape
  is a classic bootstrap script that builds `blob:` URLs from inlined
  source and injects an import map before the first module loads —
  blob URLs inherit the document's origin, so bare specifiers resolve
  and nothing is fetched. This needs proving in a browser, not
  reasoning about.
- Size: M. Source: user request.

## Done (2026-09-21)

Palette → "Share transcript (one HTML file)". One file, ~120 KB, no
network, no server, no account.

`serve/view.rs` and `serve/assets/` are `web/` now, compiled always;
only the *server* stays behind the `serve` feature. One renderer, two
outputs: `serve` fetches `/api/sessions/{id}`, `share` answers the
same paths from a table inlined in the page.

**The module graph.** The bootstrap turns each inert source into a
`blob:` URL in dependency order, rewriting bare specifiers to the URL
of the blob that satisfies them. Proved in Deno first, then in a
browser — where it failed immediately, because the list was in
*replacement* order (longest specifier first, so `"preact"` cannot
corrupt `"preact/hooks"`) when what the build needs is *dependency*
order. `hooks` was blobbed before `preact` had a URL and its bare
import survived. Two orderings, one list: dependency order in the
list, longest-first sorting inside the rewrite, and a test for each.

**What a review caught before it shipped.** The file is meant to be
sent to other people, and it was not safe to send:

- `</ScRiPt>` in a transcript closed the JSON block and everything
  after it became live markup in the reader's browser. The escape was
  a case-sensitive `replace("</script", …)`; the tokenizer matches
  case-insensitively. Reproduced in two browsers.
- `<!--` was escaped as `<\!--`, which is not valid JSON. Any
  conversation mentioning an HTML comment rendered a blank page, with
  the reason only in a console.
- Both are gone: the JSON escapes `<` as `\u003C`, which is valid,
  parses back exactly, and cannot begin a tag. The module sources stay
  verbatim — the loader parses them — guarded by a test that fails if
  anything vendored later could close its own block.
- The full-text bodies behind truncated results shipped **un-redacted**
  — the one copy that leaves the machine was the one that skipped the
  redaction `serve`'s own route documents as mandatory. And they
  shipped for *every* result, not just the cut ones, undoing the
  projection's bulk-cutting.

Also: the polls that keep a served page live are off (a file does not
change), and the session summary is one function rather than the same
eleven keys in two places.

Left, each its own issue: the delegations
([[a-share-carries-its-delegations]]), the author's paths
([[a-share-says-where-it-came-from]]), and what a share taken mid-turn
should claim ([[a-share-taken-mid-turn-says-it-is-idle]]).
