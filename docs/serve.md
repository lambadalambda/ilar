# ilar serve

```sh
ilar serve                          # http://127.0.0.1:4527/ (or an ephemeral port when taken)
ilar serve --open                   # and open a browser
ilar serve --bind 10.0.0.2:7777     # prints a URL with a token in it
```

`ilar serve` is a **separate, read-only process that tails the session
store**. It does not run turns, does not hold the writer's lease, and
does not need a provider, an API key or a model — reading a log that
already exists does not require the means to write another one. Started
anywhere on the machine it supervises every ilar process on it: the TUI,
`ilar exec`, subagents, all of them, because they all write the same
append-only JSONL.

Liveness is step-granular. Events land as steps complete, which is what
the log records; token-by-token streaming would need to be in-process
and is not what this is.

## The page

Three files compiled into the binary — an HTML shell, a stylesheet and
about 650 lines of hand-written JavaScript. No build step, no
node_modules, no CDN and no webfont: `ilar serve` works on a plane. The
server API is the durable artifact here; the page is deliberately
replaceable.

- `#/` — the session list, grouped by the directory each session was
  launched from (newest group first, newest session first within it,
  sessions that never recorded a directory in a trailing group). Each
  row shows the topic, the model, the agent, how long ago it was
  touched, and a dot for sessions written to recently.
- `#/s/<id>` — one session: a header with model, agent, directory and
  token/cost totals, then the transcript. Thoughts, tool calls and
  subagent tasks start collapsed and open on click; a task fetches its
  child's transcript only when you open it. `load earlier` walks back a
  page at a time. The view follows the tail unless you scroll up.

Rendering is plain-text-first: escaped text in `pre-wrap`, fenced blocks
to `<pre><code>`, inline backticks, bare `http(s)` URLs to links, and
`+`/`-` colouring when a tool's output reads like a diff. There is no
markdown library, and no string that came from the store is ever handed
to an HTML parser — every value goes in through `textContent`.

## Routes

Everything is `GET`. Phase 2 is read-only and the router is where that
is enforced structurally: no handler exists for any other method, so no
handler can forget to check.

| Route | What it returns |
| --- | --- |
| `GET /api/sessions` | The listing: `id`, `title`, `cwd`, `agent`, `model`, `parent_id`, `modified`, `state` (`working` / `stalled` / `idle`, derived from the turn's live scratch — not from mtime guessing), and `activity` (the running tool, e.g. `bash: cargo test`) while one is named. A long tool run stays `working` — the turn heartbeats the scratch every 20 s while a tool executes — so `stalled` means a genuinely dead or wedged process, not a slow build. Child sessions are excluded. |
| `GET /api/sessions/{id}?from=&invocation=&limit=` | One page of the transcript, newest page first: `events`, `cursor`, `has_more`, `count`, `line`, `usage`. `?invocation=<tool call id>` narrows to one subagent invocation. |
| `GET /api/sessions/{id}/events?from=&token=` | The live tail, as SSE. |
| `GET /api/sessions/{id}/children` | The sessions whose `parent_id` is this one. |
| `GET /api/sessions/{id}/results/{tool_use_id}` | The untruncated text behind a `truncated: true` tool result, as `text/plain`. |
| `GET /api/sessions/{id}/images/{event_id}/{n}` | One image's bytes. Base64 never crosses the wire in JSON; the transcript carries a descriptor and a marker line. |
| `GET /`, `/app.css`, `/app.js` | The page. |

Two cursors, deliberately different, because they count different
things. `cursor` indexes the **folded** canonical stream — what a
transcript page walks back through. `line` is the **physical** line of
the log file, monotonic forever, which is what SSE `id:` carries and
what `Last-Event-ID` or `?from=` resumes on.

Event payloads are projected server-side with the same helpers the TUI
renders with, so both surfaces cut in the same place: images become
markers plus descriptors, tool result text is bounded (with the full
text one route away), and tool inputs are summarized through the
redacting summarizers. Children are linked, never inlined.

## The SSE envelope

```text
id: 42
event: append
data: {"line":42,"event":{…}}

event: rewind    data: {"line":43,"to":7,"event":{…}}     (id: 43)
event: resync    data: {"line":43}
event: deleted   data: {}
event: error     data: {"message":"…"}
event: delta     data: {"type":"text_delta","text":"…"}   (no id)
```

`delta` frames stream the in-flight step from the turn's ephemeral
`.live` scratch: text and thinking as they generate, tool
started/finished markers. They carry no `id:` and are excluded from
`Last-Event-ID` replay — the committed event always follows on
`append`, which is when the client drops its streaming row. Thinking
text rides these frames ephemerally; it is never persisted and never
served after the step commits.

`append` and `rewind` are the only two a client folds, and the fold is
two lines, because `Rewind.to` indexes the canonical stream:

```js
if (ev.type === "rewind") events.length = ev.to;
else events.push(ev);
```

The compaction cut is a **render-time** decision. The client keeps the
whole canonical array in memory and skips the compacted-away head only
while drawing — dropping those events would shift every index a later
rewind marker points at.

`resync` means a line was missed (a lagging subscriber, a repaired tail)
and only a re-fetch is honest. `deleted` and `error` are terminal;
`error` carries the store's own words, which name the session and the
line.

Under the hood the tail is a **poll**, not a filesystem watch: on macOS
FSEvents does not report appends made through a file descriptor the
writer holds open, and ilar's writer holds one for the session's whole
life — a watch-based follower would show a frozen session and then dump
the whole conversation at process exit. See
[Sessions](sessions.md#following-a-session-as-it-is-written). A
directory scan every second drives the listing; a subscribed session is
stat-polled every 250 ms (`--poll-ms`, `ILAR_SERVE_POLL_MS`).

## Security

Read this before binding anything but loopback.

**Read-only by construction.** Every route is registered as a `GET`;
there is no code path that writes to the store, starts a turn, or runs a
tool. Anything else on a known path is a 405 from the router itself.

**Loopback by default, and no token there.** The default bind is
`127.0.0.1:4527` with no authentication, because anything that can reach
loopback on your machine can already read `~/.local/state/ilar/sessions`
directly — a token there would be theatre.

**Any other bind generates a token.** A 256-bit token, printed once, in
the fragment of the URL (`http://host:7777/#token=…`) — fragments are
not sent upstream and not written to server logs. The page moves it into
`sessionStorage`, strips it from the address bar, and sends it as a
`Bearer` header; `EventSource` and `<img>` cannot set headers, so those
carry `?token=`. Comparison is constant-time. A failure is `401` with an
empty body on **every** path, including unmatched ones, so the token is
not an oracle for which sessions exist — the three static files (`/`,
`/app.css`, `/app.js`) are the necessary exception, because the page has
to load before it can read the fragment, and they carry nothing from the
store. `ILAR_SERVE_TOKEN` pins a token
instead of generating one, and pinning it requires the check on any
bind, loopback included.

**It is plain HTTP.** The token and every transcript cross the network
in the clear. Put a non-loopback bind behind a VPN or an SSH tunnel
(`ssh -L 7777:127.0.0.1:7777 host` needs no token at all). **Never put
it on the public internet.**

**That token is the whole authorization model.** There are no users, no
roles, no per-session permissions, and no sandbox. Whoever holds it
reads every session in the store — including the transcripts of anything
your agent has ever seen, which is the most sensitive material on the
machine.
