// The page: a three-pane workspace — sessions, transcript, detail —
// built with the vendored preact + htm in /vendor. No build step, no
// npm, no CDN: the browser loads exactly what the binary carries.
//
// One rule runs through all of it, unchanged from the hand-rolled page
// this replaced: no string that came from the server is ever handed to
// an HTML parser. htm builds vnodes from *this file's* literals only —
// every server value arrives as an interpolated child or prop, which
// preact sets through createTextNode/setAttribute — and there is no
// `dangerouslySetInnerHTML` below this line. A session containing
// "<script>" renders those characters and nothing happens.

import { h, render } from "preact";
import { useCallback, useEffect, useMemo, useRef, useState } from "preact/hooks";
import htm from "htm";

const html = htm.bind(h);

// ------------------------------------------------------------- token

const TOKEN_KEY = "ilar.serve.token";

// The server prints the token in the URL fragment, which browsers do
// not send upstream. Move it into sessionStorage once and strip it from
// the address bar so it does not survive a copy-pasted link.
function bootToken() {
  const parts = location.hash.replace(/^#/, "").split("&");
  const carried = parts.find((part) => part.startsWith("token="));
  if (carried) {
    sessionStorage.setItem(TOKEN_KEY, decodeURIComponent(carried.slice(6)));
    const rest = parts.filter((part) => part && !part.startsWith("token="));
    const hash = rest.length ? "#" + rest.join("&") : "#/";
    history.replaceState(null, "", location.pathname + location.search + hash);
  }
  return sessionStorage.getItem(TOKEN_KEY) || "";
}

const token = bootToken();

// EventSource and <img> cannot set a header, so those carry ?token=.
function withToken(path) {
  if (!token) return path;
  return path + (path.includes("?") ? "&" : "?") + "token=" + encodeURIComponent(token);
}

const apiPath = (id, suffix) => "/api/sessions/" + encodeURIComponent(id) + (suffix || "");

async function fetchPath(path) {
  const headers = token ? { Authorization: "Bearer " + token } : {};
  const response = await fetch(path, { headers });
  if (!response.ok) throw new Error("GET " + path + " → " + response.status);
  return response;
}

const api = (path) => fetchPath(path).then((response) => response.json());
const apiText = (path) => fetchPath(path).then((response) => response.text());

// The write path. Every failure the server explains — a session open in
// another process, a directory that is not one, a model with no provider
// — comes back as `{error}` with a status, and both are carried on the
// thrown error because the page branches on the status (409 is a state,
// not a mishap) and shows the words.
async function post(path, body) {
  const headers = { "Content-Type": "application/json" };
  if (token) headers.Authorization = "Bearer " + token;
  const response = await fetch(path, { method: "POST", headers, body: JSON.stringify(body) });
  let data = {};
  try {
    data = await response.json();
  } catch (error) {
    data = {};
  }
  if (!response.ok) {
    const failure = new Error(data.error || "POST " + path + " → " + response.status);
    failure.status = response.status;
    throw failure;
  }
  return data;
}

const message = (error) => String((error && error.message) || error);

// What the page says about a write that worked. The server's word, in
// the user's terms: a steer is not lost, it is queued for the step the
// model is about to take.
const FATE_WORDS = {
  steering: "steering · next step",
  started: "turn started",
  aborted: "aborting …",
};

// ------------------------------------------------------------ format

// "11h 46m": two units at most, because a third is noise and one alone
// rounds a fresh session into an hour it is not in yet.
function age(iso) {
  const then = Date.parse(iso);
  if (Number.isNaN(then)) return "";
  const seconds = Math.max(0, (Date.now() - then) / 1000);
  if (seconds < 60) return Math.floor(seconds) + "s";
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return minutes + "m";
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return hours + "h " + (minutes % 60) + "m";
  const days = Math.floor(hours / 24);
  if (days < 30) return days + "d " + (hours % 24) + "h";
  return new Date(then).toISOString().slice(0, 10);
}

// The TUI's buckets, so the two surfaces quote the same number.
function cost(dollars) {
  if (dollars >= 0.995) return "$" + dollars.toFixed(2);
  if (dollars >= 0.0005) return "$" + dollars.toFixed(3);
  return dollars > 0 ? "$<0.001" : "$0.00";
}

function tokens(count) {
  return Number(count || 0).toLocaleString("en-US");
}

// Big numbers where a bar is the point: 63k, 1.2M.
function compact(count) {
  const value = Number(count || 0);
  if (value >= 1_000_000) return (value / 1_000_000).toFixed(value >= 10_000_000 ? 0 : 1) + "M";
  if (value >= 1_000) return Math.round(value / 1_000) + "k";
  return String(value);
}

const basename = (path) => String(path || "").split("/").filter(Boolean).pop() || path || "";

// A tool summary, with the session's own directory taken off the front
// of every path in it. Twenty rows all beginning
// `/Users/…/repos/thing/` say nothing twenty times; the directory is in
// the panel, and the untouched summary stays in the row's title.
function shorten(summary, cwd) {
  const text = String(summary || "");
  if (!cwd) return text;
  return text.split(cwd + "/").join("").split(cwd).join(".");
}

// The tokens a turn was holding: the wire's four counters, which is what
// `Usage::context_tokens` adds up for every provider that reports cached
// input separately.
function contextTokens(usage) {
  if (!usage) return 0;
  return (
    Number(usage.input || 0) +
    Number(usage.cache_read || 0) +
    Number(usage.cache_creation || 0) +
    Number(usage.output || 0)
  );
}

// ------------------------------------------------------- text render

// Plain text first: fenced blocks, inline backticks, paired **bold**,
// bare http(s) links and diff colouring. Everything else is left exactly
// as written — a lone asterisk stays an asterisk, because guessing at
// markdown in a shell transcript is how a path turns into italics.
// **…** is the exception because models emit it constantly (every
// reasoning summary opens with one) and the raw markers are noise on
// screen; the pair must open and close on one line against non-space,
// so a glob or a footnote marker is not bold. Both quantifiers in the
// body are lazy — a greedy one would match `**a** and **b**` as a
// single span and put the seam it swallowed back on screen — and the
// bound keeps one very long line with an unclosed `**` from costing a
// scan to the end of it per marker.
const INLINE = /`([^`\n]+)`|\*\*(\S(?:[^\n]{0,400}?\S)??)\*\*|(https?:\/\/[^\s<>"'`)\]]+)/g;
const TRAILING = /[.,;:!?]+$/;

// Tokenize into a child array: plain strings and vnodes only, so every
// span the server sent lands as a text node.
function inline(text) {
  const source = String(text === null || text === undefined ? "" : text);
  const out = [];
  let last = 0;
  for (const match of source.matchAll(INLINE)) {
    if (match.index > last) out.push(source.slice(last, match.index));
    if (match[1] !== undefined) {
      out.push(html`<code class="inline">${match[1]}</code>`);
    } else if (match[2] !== undefined) {
      // Recursed, so a link or a backticked flag inside a bold span is
      // still a link and still a code span. The body cannot contain a
      // closed pair, so this bottoms out at one level.
      out.push(html`<strong class="bold">${inline(match[2])}</strong>`);
    } else {
      // The regex admits http(s) only, so no javascript: URL can reach
      // an href here.
      const url = match[3].replace(TRAILING, "");
      out.push(
        html`<a class="link" href=${url} target="_blank" rel="noreferrer noopener">${url}</a>`,
      );
      out.push(match[3].slice(url.length));
    }
    last = match.index + match[0].length;
  }
  out.push(source.slice(last));
  return out;
}

function richText(text) {
  const lines = String(text === null || text === undefined ? "" : text).split("\n");
  const out = [];
  let buffer = [];
  let fenced = null;
  const flush = () => {
    const body = buffer.join("\n");
    if (body.trim()) out.push(html`<div class="text">${inline(body)}</div>`);
    buffer = [];
  };
  for (const line of lines) {
    const fence = /^\s*```/.test(line);
    if (fenced !== null) {
      if (fence) {
        out.push(html`<pre class="code"><code>${fenced.join("\n")}</code></pre>`);
        fenced = null;
      } else {
        fenced.push(line);
      }
    } else if (fence) {
      flush();
      fenced = [];
    } else {
      buffer.push(line);
    }
  }
  if (fenced !== null) out.push(html`<pre class="code"><code>${fenced.join("\n")}</code></pre>`);
  flush();
  return out;
}

function looksLikeDiff(text) {
  if (/^(@@ |diff --git |--- |\+\+\+ )/m.test(text)) return true;
  const lines = text.split("\n");
  const added = lines.filter((line) => /^\+(?!\+)/.test(line)).length;
  const removed = lines.filter((line) => /^-(?!-)/.test(line)).length;
  return added > 0 && removed > 0 && added + removed >= lines.length * 0.3;
}

function diffKind(line) {
  if (line.startsWith("@@")) return "hunk";
  if (/^\+(?!\+\+)/.test(line)) return "add";
  if (/^-(?!--)/.test(line)) return "del";
  return "ctx";
}

// A tool's detail or result: a diff if it reads like one, plain
// pre-wrap otherwise. Never markdown — this is program output.
// No whitespace inside the tags: this is a <pre>, and a prettier
// template literal would be indentation on screen.
//
// Both answers are memoized on the text itself. `looksLikeDiff` walks
// the whole body three times and the split walks it again, and an open
// row is re-rendered on every frame of a streaming turn — an unmemoized
// row makes a 200 kB result cost four scans per animation frame for an
// answer that cannot change while the string does not.
function Preformatted({ text, className }) {
  const body = String(text || "");
  const diff = useMemo(() => looksLikeDiff(body), [body]);
  const lines = useMemo(
    () =>
      diff
        ? body.split("\n").map((line) => html`<span class=${diffKind(line)}>${line + "\n"}</span>`)
        : null,
    [body, diff],
  );
  if (!diff) return html`<pre class=${className || "detail"}>${body}</pre>`;
  return html`<pre class="diff">${lines}</pre>`;
}

// The first line of something long, for a row that is still collapsed.
function preview(text, width) {
  const line = String(text || "").split("\n").find((one) => one.trim()) || "";
  const cap = width || 90;
  // Unconditionally: the cut is one way to orphan a marker, and a bold
  // span the model wrapped across two lines is the other.
  return unpaired(line.length > cap ? line.slice(0, cap - 1) + "…" : line);
}

// A first line that ends inside a **bold** span keeps its opening
// marker, and an unpaired marker renders as the two asterisks it is.
// Drop the orphan and keep its words.
function unpaired(cut) {
  const marks = cut.match(/\*\*/g);
  if (!marks || marks.length % 2 === 0) return cut;
  const at = cut.lastIndexOf("**");
  return cut.slice(0, at) + cut.slice(at + 2);
}

// ------------------------------------------------------------- glyphs

// One glyph per tool family. Text, not an icon font: the page has no
// webfont and must work on a plane.
const GLYPHS = {
  read: "▤",
  write: "✎",
  edit: "✎",
  patch: "✎",
  apply_patch: "✎",
  bash: "▸",
  shell: "▸",
  grep: "⌕",
  search: "⌕",
  glob: "⌕",
  list: "☰",
  todo: "☑",
  task: "◆",
  web: "◍",
  fetch: "◍",
};

function glyph(name) {
  const key = String(name || "").toLowerCase();
  return GLYPHS[key] || GLYPHS[key.split("_")[0]] || "•";
}

// The tool name as a person says it: `apply_patch` is "Apply Patch".
function toolTitle(name) {
  return String(name || "tool")
    .split(/[_\-\s]+/)
    .filter(Boolean)
    .map((word) => word.charAt(0).toUpperCase() + word.slice(1))
    .join(" ");
}

// --------------------------------------------------------------- rows

function Images({ sessionId, eventId, descriptors }) {
  if (!descriptors || !descriptors.length) return null;
  return descriptors.map(
    (image) => html`
      <img
        class="thumb"
        loading="lazy"
        alt=${image.media_type + " · " + tokens(image.bytes) + " bytes"}
        src=${withToken(apiPath(sessionId, "/images/" + encodeURIComponent(eventId) + "/" + image.n))}
      />
    `,
  );
}

// One line per tool call: glyph, name, muted argument summary. The whole
// row is the affordance — clicking it opens the input detail and the
// result, and a truncated result is fetched whole from its own route
// only once someone asks for it.
function ToolRow({ call, result, sessionId, cwd }) {
  const [open, setOpen] = useState(false);
  const [full, setFull] = useState(null);
  const truncated = result && result.truncated;

  useEffect(() => {
    if (!open || !truncated || full !== null) return;
    let live = true;
    apiText(apiPath(sessionId, "/results/" + encodeURIComponent(result.tool_use_id)))
      .then((text) => live && setFull(text))
      .catch((error) => live && setFull("could not load the full result: " + message(error)));
    return () => {
      live = false;
    };
  }, [open, truncated, full, sessionId, result && result.tool_use_id]);

  const state = !result ? "run" : result.is_error ? "err" : "ok";
  const body = open
    ? html`
        <div class="tool-body">
          ${call.detail && html`<${Preformatted} text=${call.detail} />`}
          ${!result && html`<p class="note">running…</p>`}
          ${result &&
          (result.text || full) &&
          html`<${Preformatted}
            text=${full === null ? result.text : full}
            className=${result.is_error ? "detail error" : "detail"}
          />`}
          ${result && truncated && full === null && html`<p class="note">loading full result…</p>`}
          ${result &&
          html`<${Images}
            sessionId=${sessionId}
            eventId=${result.id}
            descriptors=${result.images}
          />`}
        </div>
      `
    : null;

  return html`
    <div class=${"tool " + state + (open ? " open" : "")}>
      <button class="tool-line" type="button" onClick=${() => setOpen(!open)}>
        <span class=${"glyph " + state}>${!result ? html`<span class="spinner" />` : glyph(call.name)}</span>
        <span class="tool-name">${toolTitle(call.name)}</span>
        ${call.agent && call.agent.name && html`<span class="chip">${call.agent.name}</span>`}
        <span class="tool-args" title=${call.summary || ""}
          >${inline(preview(shorten(call.summary, cwd), 140))}</span
        >
      </button>
      ${body}
    </div>
  `;
}

// A task row's detail is a whole child transcript, fetched only when
// someone opens it — the parent log holds a link, never the turns.
function TaskRow({ call, result, sessionId, cwd }) {
  const [open, setOpen] = useState(false);
  const child = result && result.child_session_id;
  const [page, setPage] = useState(null);
  const [error, setError] = useState("");

  useEffect(() => {
    if (!open || !child || page) return;
    let live = true;
    api(apiPath(child, "?invocation=" + encodeURIComponent(call.id)))
      .then((loaded) => live && setPage(loaded))
      .catch((failure) => live && setError(message(failure)));
    return () => {
      live = false;
    };
  }, [open, child, page, call.id]);

  const state = !result ? "run" : result.is_error ? "err" : "ok";
  return html`
    <div class=${"tool task " + state + (open ? " open" : "")}>
      <button class="tool-line" type="button" onClick=${() => setOpen(!open)}>
        <span class=${"glyph " + state}>${!result ? html`<span class="spinner" />` : "◆"}</span>
        <span class="tool-name">Task</span>
        ${call.agent && call.agent.name && html`<span class="chip">${call.agent.name}</span>`}
        <span class="tool-args" title=${call.summary || ""}
          >${inline(preview(shorten(call.summary, cwd), 140))}</span
        >
      </button>
      ${open &&
      html`
        <div class="tool-body">
          ${call.detail && html`<${Preformatted} text=${call.detail} />`}
          ${!child && html`<p class="note">${result ? "no child session recorded" : "running…"}</p>`}
          ${error && html`<p class="note">${error}</p>`}
          ${child &&
          !page &&
          !error &&
          html`<p class="note">loading…</p>`}
          ${page &&
          html`
            <div class="child">
              ${pageRows(page.events, page.cursor, child, cwd)}
              <a class="link" href=${"#/s/" + encodeURIComponent(child)}>open the child session</a>
            </div>
          `}
        </div>
      `}
    </div>
  `;
}

// Reasoning, dim and closed: the title a model writes is `**Planning…**`,
// which is prose, so it renders as prose rather than as its own markers.
// A compaction summary wears the same row — it is the same gesture, a
// long thing folded to one line.
function ThinkingRow({ text, label }) {
  const [open, setOpen] = useState(false);
  return html`
    <div class=${"thought" + (open ? " open" : "")}>
      <button class="thought-line" type="button" onClick=${() => setOpen(!open)}>
        <span class="glyph">${open ? "▾" : "▸"}</span>
        <span class="thought-label">${label || "thinking"}</span>
        <span class="thought-preview">${inline(preview(text, 120))}</span>
      </button>
      ${open && html`<div class="thought-body">${richText(text)}</div>`}
    </div>
  `;
}

// The compaction cut, applied at render time and only here. The full
// canonical array stays in memory because `Rewind.to` indexes it: drop
// the compacted-away head and every later rewind would truncate to the
// wrong place.
//
// `kept_from` indexes the whole log while `events` starts at `base`, so
// it is rebased here; the clamp to the compaction's own position is
// what keeps a slice whose base is unknown (a child invocation) honest,
// since everything before a compaction is compacted away anyway.
function compactionCut(events, base) {
  let cut = 0;
  for (let index = 0; index < events.length; index += 1) {
    const event = events[index];
    if (event.type === "compaction") {
      cut = Math.max(cut, Math.min((event.kept_from || 0) - base, index));
    }
  }
  return Math.max(0, cut);
}

// `offset` is the canonical index of `events[0]`, and it is what makes
// the fallback key stable: "load earlier" prepends a page, which shifts
// every position in the sliced array by the page's length and lowers
// `base` by exactly the same amount. A key carrying the sliced index
// would therefore change on every surviving row — preact would unmount
// the list, collapsing open tool rows and throwing away results already
// fetched — while the canonical index does not move.
function eventRows(events, sessionId, cwd, offset) {
  const results = new Map();
  for (const event of events) {
    if (event.type === "tool_result") results.set(event.tool_use_id, event);
  }
  const consumed = new Set();
  const rows = [];
  const base = Number(offset || 0);
  events.forEach((event, index) => {
    const key = event.id || "@" + (base + index);
    switch (event.type) {
      case "user_message":
        rows.push(html`
          <div class="block user" key=${key}>
            <div class="who">you</div>
            ${richText(event.text)}
            <${Images} sessionId=${sessionId} eventId=${event.id} descriptors=${event.images} />
          </div>
        `);
        break;
      case "assistant_message":
        (event.content || []).forEach((block, at) => {
          const inner = key + ":" + at;
          if (block.type === "text") {
            rows.push(html`<div class="block assistant" key=${inner}>${richText(block.text)}</div>`);
          } else if (block.type === "reasoning_summary") {
            rows.push(html`<${ThinkingRow} key=${inner} text=${block.text} />`);
          } else if (block.type === "tool_call") {
            const result = results.get(block.id);
            if (result) consumed.add(result.id);
            const Row = block.name === "task" ? TaskRow : ToolRow;
            rows.push(
              html`<${Row}
                key=${inner}
                call=${block}
                result=${result}
                sessionId=${sessionId}
                cwd=${cwd}
              />`,
            );
          }
        });
        break;
      case "tool_result":
        // Only a result whose call is off the top of this page: the rest
        // render inside the call they answer.
        if (consumed.has(event.id)) break;
        rows.push(html`
          <div class=${"block result" + (event.is_error ? " error" : "")} key=${key}>
            <div class="who">result</div>
            ${event.text && html`<${Preformatted} text=${event.text} />`}
            <${Images} sessionId=${sessionId} eventId=${event.id} descriptors=${event.images} />
          </div>
        `);
        break;
      case "model_change":
        rows.push(html`
          <div class="note-row" key=${key}>
            model → ${event.model}${event.variant ? " · " + event.variant : ""}
          </div>
        `);
        break;
      case "compaction":
        rows.push(html`<${ThinkingRow} key=${key} label="compacted" text=${event.summary} />`);
        break;
      case "rewind":
        rows.push(html`<div class="divider" key=${key}>rewound to event ${event.to}</div>`);
        break;
      default:
        // meta, checkpoint, topic, subagent_invocation: state, not
        // transcript. They keep their index and render nothing.
        break;
    }
  });
  return rows;
}

// A window onto a log — the centre pane's, or a child invocation's —
// cut at its compaction and keyed against its own canonical origin.
function pageRows(events, cursor, sessionId, cwd) {
  const cut = compactionCut(events, cursor);
  return eventRows(events.slice(cut), sessionId, cwd, cursor + cut);
}

// ------------------------------------------------- streaming tail row

// What the running step has produced so far, rebuilt from `delta`
// frames. Never folded into the canonical events: it is a stand-in for a
// step nobody has committed, dropped the moment the real event arrives.
//
// `thoughts` is a list because the wire says where one thought ends —
// a `thinking_break` per closed summary. Appending them all to one
// string is what used to run a step's whole reasoning together into a
// single paragraph seamed with stray `**`.
function liveApply(previous, data) {
  if (data.type === "reset") return null;
  const live = previous || { text: "", thoughts: [], tools: [] };
  if (data.type === "text_delta") live.text += data.text;
  else if (data.type === "thinking_delta") {
    if (!live.thoughts.length) live.thoughts.push("");
    live.thoughts[live.thoughts.length - 1] += data.text;
  } else if (data.type === "thinking_break") {
    if (live.thoughts.length && live.thoughts[live.thoughts.length - 1].trim()) live.thoughts.push("");
  } else if (data.type === "tool_started") {
    live.tools.push({ id: data.id, name: data.name, summary: data.summary });
  } else if (data.type === "tool_finished") {
    const tool = live.tools.find((one) => one.id === data.id);
    if (tool) tool.ok = data.ok;
  } else if (data.type === "turn_started") {
    return { text: "", thoughts: [], tools: [] };
  }
  return live;
}

// The in-flight step: one container, shaped like the transcript rows it
// is about to become. Everything the step has said so far lives inside
// it — the thoughts it closed, the text it is writing, the tools it
// started — so a running turn is one thing on screen.
function LiveStep({ live }) {
  if (!live) return null;
  const thoughts = live.thoughts.filter((one) => one.trim());
  const writing = live.text.trim();
  if (!thoughts.length && !writing && !live.tools.length) return null;
  // Exactly one thing says "this is what is happening now": the spinner
  // while a tool runs, the caret on the newest words otherwise.
  const busy = live.tools.some((tool) => tool.ok === undefined);
  return html`
    <div class="live">
      ${thoughts.map(
        (thought, index) => html`
          <div class=${"live-thought" + (!busy && !writing && index === thoughts.length - 1 ? " active" : "")}>
            <span class="glyph">▸</span>${inline(preview(thought, 120))}
          </div>
        `,
      )}
      ${writing && html`<div class=${"live-text" + (busy ? "" : " active")}>${richText(live.text)}</div>`}
      ${live.tools.map(
        (tool) => html`
          <div class=${"tool-line live-tool " + (tool.ok === undefined ? "run" : tool.ok ? "ok" : "err")}>
            <span class="glyph">
              ${tool.ok === undefined ? html`<span class="spinner" />` : glyph(tool.name)}
            </span>
            <span class="tool-name">${toolTitle(tool.name)}</span>
            <span class="tool-args">${inline(preview(tool.summary, 140))}</span>
          </div>
        `,
      )}
    </div>
  `;
}

// ---------------------------------------------------------- transcript

function blankView(id) {
  return {
    id,
    events: [],
    base: 0,
    hasMore: false,
    line: 0,
    live: null,
    rewound: null,
    session: null,
    usage: {},
    count: 0,
    status: id ? "loading…" : "",
    error: "",
    // Whether the error on screen is one a button can do anything
    // about: a stream that gave up, or a page that failed to load. A
    // deleted session is not.
    retryable: false,
    retry: null,
    loading: Boolean(id),
    pending: false,
    earlier: null,
  };
}

// How long to wait before each hand-rolled reconnect, and — because the
// list ends — how many times to try before saying the stream is gone
// rather than going on claiming it is coming back.
const RECONNECT_WAITS = [300, 800, 1500, 3000];

// One open session: the canonical folded stream from `base` onward, the
// physical line the tail stands at (which is what the SSE stream resumes
// from), and the running step's scratch.
//
// The state is a mutable object behind a ref rather than useState: the
// fold is index arithmetic over an array that a rewind truncates in
// place, and copying it per frame would make a busy turn quadratic. A
// rAF-coalesced counter is what asks preact to look again.
function useTranscript(id) {
  const store = useRef(blankView(null));
  const [, bump] = useState(0);
  const scheduled = useRef(false);
  const paint = useCallback(() => {
    if (scheduled.current) return;
    scheduled.current = true;
    requestAnimationFrame(() => {
      scheduled.current = false;
      bump((count) => count + 1);
    });
  }, []);

  useEffect(() => {
    const view = blankView(id);
    store.current = view;
    paint();
    if (!id) return undefined;

    let alive = true;
    let source = null;
    let waiting = null;
    let attempts = 0;
    // Which fetch of the page the view currently describes. Every load
    // takes a number and a reply from an older one is dropped, so a
    // resync's reload cannot be overwritten by the page a "load earlier"
    // asked for against the window that reload replaced.
    let generation = 0;

    const detach = () => {
      if (source) {
        source.close();
        source = null;
      }
      if (waiting) {
        clearTimeout(waiting);
        waiting = null;
      }
    };

    // The design's fold, over the canonical array, with the one
    // adjustment paging forces: index 0 of what we hold is canonical
    // index `base`. Returns what it did, because the caller's answer to
    // "nothing" differs by kind.
    const fold = (kind, data) => {
      // Idempotence. Every line-bearing frame carries the line it puts
      // the client on, and ours only moves forward — so a frame at or
      // below where we stand is one we have already folded, which is
      // exactly what a resumed stream replays. Appending it again would
      // duplicate the transcript on every reconnect.
      if (typeof data.line !== "number" || data.line <= view.line) return "stale";
      if (kind === "rewind") {
        if (data.to < view.base) return "gone";
        // Clamped: `to` is an index into the whole log and this client
        // may hold less of it than the rewind cuts to (a short page, a
        // window that starts later). Assigning a longer length would
        // extend the array with holes, and the next render would read
        // `.type` off `undefined`.
        const kept = Math.min(view.events.length, data.to - view.base);
        view.rewound = Math.max(0, view.events.length - kept);
        view.events.length = kept;
      } else {
        view.rewound = null;
        view.events.push(data.event);
      }
      view.line = data.line;
      return "applied";
    };

    // The stream gave up. Not "reconnecting…", which would be a claim
    // that something is still trying.
    const offline = (why) => {
      detach();
      view.live = null;
      view.status = "offline";
      view.error = why;
      view.retryable = true;
      paint();
    };

    const reconnect = () => {
      const wait = RECONNECT_WAITS[Math.min(attempts, RECONNECT_WAITS.length - 1)];
      attempts += 1;
      detach();
      // Deltas are not resumed (they carry no id), so the half-message
      // on screen would silently gain a hole.
      view.live = null;
      view.status = "reconnecting…";
      paint();
      waiting = setTimeout(() => {
        waiting = null;
        if (alive) attach();
      }, wait);
    };

    // The reconnect is ours rather than the browser's on purpose. An
    // EventSource retries the URL it was *constructed* with, so its own
    // reconnect would ask for `?from=` the line this tab attached at and
    // be sent everything since — onto a client that already holds it.
    // Reopening by hand is what makes the query say where we actually
    // stand; `fold` refuses a replayed line either way.
    const attach = () => {
      detach();
      const stream = new EventSource(withToken(apiPath(id, "/events?from=" + view.line)));
      source = stream;
      const on = (name, handler) =>
        stream.addEventListener(name, (frame) => {
          if (!alive || source !== stream) return;
          handler(JSON.parse(frame.data));
        });
      stream.addEventListener("open", () => {
        if (!alive || source !== stream) return;
        attempts = 0;
        view.status = "live";
        view.error = "";
        view.retryable = false;
        paint();
      });
      // The server's terminal `error` frame and `EventSource`'s own
      // transport failure arrive under the same name; only the server's
      // carries data.
      stream.addEventListener("error", (frame) => {
        if (!alive || source !== stream) return;
        if (frame && typeof frame.data === "string") {
          detach();
          view.status = "stopped";
          view.error = JSON.parse(frame.data).message;
          view.retryable = true;
          paint();
        } else if (stream.readyState === EventSource.CLOSED) {
          // Fatal: a non-200 or a wrong content type closes an
          // EventSource for good. Nothing is coming back on its own.
          offline("The live stream was refused — reload or retry.");
        } else if (attempts >= RECONNECT_WAITS.length) {
          offline("Lost the live stream to this session — the server may be gone.");
        } else {
          reconnect();
        }
      });
      on("append", (data) => {
        if (fold("append", data) !== "applied") return;
        // The committed step supersedes the stand-in for it. Only that
        // one: a tool result lands while the *other* tools of the same
        // step are still streaming, and dropping the row there would
        // lose them.
        if (data.event && data.event.type === "assistant_message") view.live = null;
        view.count += 1;
        paint();
      });
      // The running turn's scratch. Ephemeral by design — no id, no
      // replay.
      on("delta", (data) => {
        view.live = liveApply(view.live, data);
        paint();
      });
      on("rewind", (data) => {
        // A rewind below the loaded window leaves nothing to fold onto.
        const outcome = fold("rewind", data);
        if (outcome === "gone") reload();
        else if (outcome === "applied") paint();
      });
      // The view is stale — a lagging subscriber or a repaired tail.
      // Only a re-fetch is honest.
      on("resync", () => reload());
      on("deleted", () => {
        detach();
        view.status = "deleted";
        view.error = "This session was deleted.";
        view.retryable = false;
        paint();
      });
    };

    const load = async () => {
      const mine = (generation += 1);
      try {
        const page = await api(apiPath(id));
        if (!alive || mine !== generation) return;
        view.events = page.events || [];
        view.base = page.cursor;
        view.hasMore = page.has_more;
        view.line = page.line;
        view.session = page.session || null;
        view.usage = page.usage || {};
        view.count = page.count;
        view.loading = false;
        view.error = "";
        view.retryable = false;
        paint();
        attach();
      } catch (error) {
        if (!alive || mine !== generation) return;
        view.loading = false;
        view.status = "";
        view.error = message(error);
        view.retryable = true;
        paint();
      }
    };

    const reload = () => {
      detach();
      attempts = 0;
      view.live = null;
      view.rewound = null;
      view.loading = true;
      view.retryable = false;
      paint();
      load();
    };

    view.retry = reload;

    view.earlier = async () => {
      if (view.pending || !view.hasMore) return;
      const mine = generation;
      const from = view.base;
      view.pending = true;
      paint();
      try {
        const page = await api(apiPath(id, "?from=" + from));
        // A reload landed while this page was in flight: it holds a
        // window this head no longer sits above, and prepending would
        // paste a stale head onto a fresh base.
        if (!alive || mine !== generation || view.base !== from) return;
        view.events = page.events.concat(view.events);
        view.base = page.cursor;
        view.hasMore = page.has_more;
      } catch (error) {
        if (alive && mine === generation) view.error = message(error);
      } finally {
        view.pending = false;
        paint();
      }
    };

    load();
    return () => {
      alive = false;
      detach();
    };
  }, [id, paint]);

  // The effect runs after the render that changed `id`, so without this
  // the first frame of a new session would draw the old one's events
  // under the new one's header.
  if (store.current.id !== id) store.current = blankView(id);
  return store.current;
}

// --------------------------------------------------------- the panes

function Dot({ state, title }) {
  return html`<span class=${"dot " + (state || "idle")} title=${title || state || "idle"} />`;
}

function SessionGroup({ cwd, rows, current, onPick }) {
  const [open, setOpen] = useState(true);
  return html`
    <section class="group">
      <button class="group-head" type="button" title=${cwd || "sessions with no directory"} onClick=${() => setOpen(!open)}>
        <span class="glyph">${open ? "▾" : "▸"}</span>
        <span class="group-name">${cwd ? basename(cwd) : "elsewhere"}</span>
        <span class="group-count">${rows.length}</span>
      </button>
      ${open &&
      rows.map(
        (session) => html`
          <a
            key=${session.id}
            class=${"session" + (session.id === current ? " current" : "")}
            href=${"#/s/" + encodeURIComponent(session.id)}
            title=${session.cwd || ""}
            onClick=${() => onPick && onPick()}
          >
            <${Dot}
              state=${session.state}
              title=${session.state === "stalled"
                ? "a turn is running but has not written for a minute"
                : session.state}
            />
            <span class="session-title">${inline(preview(session.title || session.id, 70))}</span>
            <span class="session-age">${age(session.modified)}</span>
          </a>
        `,
      )}
    </section>
  `;
}

// Newest group first, with the sessions that never named a directory
// last: they have nothing to group by, not the newest nothing.
function grouped(sessions) {
  const groups = new Map();
  for (const session of sessions) {
    const key = session.cwd || "";
    if (!groups.has(key)) groups.set(key, []);
    groups.get(key).push(session);
  }
  const when = (session) => Date.parse(session.modified) || 0;
  const ordered = [];
  for (const [cwd, rows] of groups) {
    rows.sort((left, right) => when(right) - when(left));
    ordered.push({ cwd, rows, newest: when(rows[0]) });
  }
  ordered.sort(
    (left, right) => (left.cwd === "") - (right.cwd === "") || right.newest - left.newest,
  );
  return ordered;
}

// Start a session from the page. The three things a launch decides —
// what to ask, where it runs, which model — and no more: everything else
// is what `ilar` itself would resolve from configuration, which is the
// point of the server running the same runtime the TUI does.
//
// The directory is a free text field with the store's own directories
// suggested, because the browser cannot see the filesystem and a picker
// that only offered previous directories would make the first session in
// a new project impossible to start.
function NewSession({ cwds, onCreated }) {
  const [open, setOpen] = useState(false);
  const [prompt, setPrompt] = useState("");
  const [cwd, setCwd] = useState("");
  const [model, setModel] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  const submit = async (event) => {
    event.preventDefault();
    if (!prompt.trim() || busy) return;
    setBusy(true);
    setError("");
    try {
      const created = await post("/api/sessions", {
        prompt: prompt.trim(),
        cwd: cwd.trim() || null,
        model: model.trim() || null,
      });
      setPrompt("");
      setOpen(false);
      onCreated(created.id);
    } catch (failure) {
      setError(message(failure));
    } finally {
      setBusy(false);
    }
  };

  if (!open) {
    return html`
      <button class="new-toggle" type="button" onClick=${() => setOpen(true)}>+ new session</button>
    `;
  }
  return html`
    <form class="new-session" onSubmit=${submit}>
      <textarea
        class="new-prompt"
        rows="3"
        required
        autofocus
        placeholder="what should it do?"
        value=${prompt}
        onInput=${(event) => setPrompt(event.target.value)}
      ></textarea>
      <input
        class="new-field"
        list="known-cwds"
        placeholder="working directory (this server's, if blank)"
        value=${cwd}
        onInput=${(event) => setCwd(event.target.value)}
      />
      <datalist id="known-cwds">
        ${cwds.map((known) => html`<option key=${known} value=${known}></option>`)}
      </datalist>
      <input
        class="new-field"
        placeholder="config default"
        value=${model}
        onInput=${(event) => setModel(event.target.value)}
      />
      ${error && html`<p class="note error">${error}</p>`}
      <div class="new-actions">
        <button type="button" class="ghost" onClick=${() => setOpen(false)}>cancel</button>
        <button type="submit" disabled=${busy || !prompt.trim()}>${busy ? "starting…" : "start"}</button>
      </div>
    </form>
  `;
}

function Sidebar({ sessions, current, error, onPick, onCreated }) {
  const groups = useMemo(() => grouped(sessions), [sessions]);
  const working = sessions.filter((session) => session.state === "working").length;
  const cwds = useMemo(
    () => [...new Set(sessions.map((session) => session.cwd).filter(Boolean))].sort(),
    [sessions],
  );
  return html`
    <aside class="sidebar">
      <header class="sidebar-head">
        <a class="brand" href="#/">ilar</a>
        <span class="sidebar-count">
          ${sessions.length} session${sessions.length === 1 ? "" : "s"}
          ${working > 0 && html`<span class="working"> · ${working} working</span>`}
        </span>
      </header>
      <div class="sidebar-new">
        <${NewSession} cwds=${cwds} onCreated=${onCreated} />
      </div>
      <div class="sidebar-body">
        ${error && html`<p class="note">${error}</p>`}
        ${!error && !sessions.length && html`<p class="note">No sessions in this store yet.</p>`}
        ${groups.map(
          (group) => html`
            <${SessionGroup}
              key=${group.cwd}
              cwd=${group.cwd}
              rows=${group.rows}
              current=${current}
              onPick=${onPick}
            />
          `,
        )}
      </div>
    </aside>
  `;
}

// The strip above the input box: what the session is doing, what it has
// spent doing it, and — only for a turn this server is running — the
// control that stops it. A session working under a TUI shows the same
// dot and no button: nothing here can stop that one.
function StatusPill({ view, session, onAbort, aborting }) {
  const state = (session && session.state) || (view.live ? "working" : "idle");
  const model = (session && session.model) || (view.session && view.session.model) || "";
  const activity = session && session.activity;
  const usage = view.usage || {};
  // `status` is the stream's own word — "reconnecting…", "stopped",
  // "deleted" — and only worth saying when it is not the ordinary
  // "live", which would read as a claim about the session rather than
  // about the socket.
  const connection = view.status && view.status !== "live" ? view.status : "";
  const saying =
    state === "working"
      ? activity || (model ? model + " is thinking …" : "working …")
      : state === "stalled"
        ? "quiet since the last write"
        : connection || "idle";
  return html`
    <div class=${"pill " + state}>
      <${Dot} state=${state} />
      <span class="pill-say">${saying}</span>
      <span class="pill-facts">
        ${view.count} events · in ${compact(usage.input)} · out ${compact(usage.output)}
        ${typeof usage.cost_dollars === "number"
          ? " · " + cost(usage.cost_dollars)
          : usage.plan
            ? " · plan"
            : ""}
      </span>
      ${session &&
      session.driven &&
      html`
        <button class="abort" type="button" disabled=${aborting} onClick=${onAbort} title="stop this turn">
          ${aborting ? "stopping…" : "stop"}
        </button>
      `}
    </div>
  `;
}

// The input box, in the spot the status pill was holding for it.
//
// Enter sends and Shift-Enter breaks the line, which is the shape every
// chat surface has taught; the button is there for a phone. What the
// send *did* is the server's word, shown briefly rather than as a row in
// the transcript — the transcript is the session's, and a steer already
// appears there when the model receives it.
//
// A 409 is not a failure to retry: the session belongs to another
// process, and until that changes this tab is a watcher. It says so —
// but it does not lock, because the lock it used to take had no way out.
// The lease is transient (a TUI that later exits), the page cannot see
// it drop, and the only thing that clears the notice was a successful
// send through a box the notice had already disabled. So the box stays
// usable, the next success clears the notice, and so does any change in
// what the listing says the session is doing.
//
// This component is keyed on the session id by its parent, which is what
// makes the awaits below safe: a sidebar switch remounts it, so an
// in-flight POST resolves into the instance that started it — a dead one
// whose `setText("")` reaches nobody — rather than clearing the draft
// someone has just typed into the session they moved to.
function Composer({ id, session, view }) {
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const [aborting, setAborting] = useState(false);
  const [fate, setFate] = useState("");
  const [refused, setRefused] = useState(false);

  const driven = session ? session.driven : null;
  const state = session ? session.state : null;
  // The lease is invisible from here, but the process holding it cannot
  // exit without the listing moving: take any change in the session's
  // liveness as reason to stop asserting a refusal we cannot re-check.
  useEffect(() => {
    setRefused(false);
  }, [driven, state]);

  // Say it and let it go: a stale "turn started" under a finished turn
  // reads as a claim about now.
  useEffect(() => {
    if (!fate) return undefined;
    const timer = setTimeout(() => setFate(""), 4000);
    return () => clearTimeout(timer);
  }, [fate]);

  const send = useCallback(async () => {
    const body = text.trim();
    if (!body || busy) return;
    setBusy(true);
    try {
      const result = await post(apiPath(id, "/message"), { text: body });
      // Only the words that were sent: a send is not instant, and
      // whatever was typed on top of them is still a draft.
      setText((current) => (current.trim() === body ? "" : current));
      setRefused(false);
      setFate(FATE_WORDS[result.fate] || result.fate || "sent");
    } catch (error) {
      if (error.status === 409) setRefused(true);
      setFate(message(error));
    } finally {
      setBusy(false);
    }
  }, [text, busy, id]);

  const abort = useCallback(async () => {
    setAborting(true);
    try {
      const result = await post(apiPath(id, "/abort"), {});
      setFate(FATE_WORDS[result.fate] || result.fate || "aborted");
    } catch (error) {
      setFate(message(error));
    } finally {
      setAborting(false);
    }
  }, [id]);

  const onKeyDown = (event) => {
    if (event.key !== "Enter" || event.shiftKey) return;
    event.preventDefault();
    send();
  };

  return html`
    ${refused &&
    html`<p class="watching">
      This session is open in another process — watching only. Send again to try once it lets go.
    </p>`}
    <${StatusPill} view=${view} session=${session} onAbort=${abort} aborting=${aborting} />
    <div class="input">
      <textarea
        class="prompt"
        rows="1"
        placeholder="message this session — Enter sends, Shift-Enter for a new line"
        value=${text}
        onInput=${(event) => setText(event.target.value)}
        onKeyDown=${onKeyDown}
      ></textarea>
      <button class="send" type="button" disabled=${busy || !text.trim()} onClick=${send} title="send">
        ${busy ? "…" : "send"}
      </button>
    </div>
    ${fate && html`<p class="fate">${fate}</p>`}
  `;
}

function Card({ title, extra, children }) {
  return html`
    <section class="card">
      <header class="card-head">
        <h2>${title}</h2>
        ${extra !== undefined && extra !== null && html`<span class="card-extra">${extra}</span>`}
      </header>
      ${children}
    </section>
  `;
}

// The context bar: what the newest turn was holding against the window
// the model actually accepts. `context_limit` is null for a model this
// binary has no catalog row for, and then there is no honest bar to draw.
function ContextBar({ used, limit }) {
  if (!limit) {
    return html`<div class="fact"><span>Context</span><span>${compact(used)} tokens</span></div>`;
  }
  const share = Math.min(1, used / limit);
  const level = share > 0.9 ? "hot" : share > 0.7 ? "warm" : "cool";
  return html`
    <div class="context">
      <div class="fact">
        <span>Context</span>
        <span>${(share * 100).toFixed(1)}%</span>
      </div>
      <div class="bar" title=${tokens(used) + " of " + tokens(limit) + " tokens"}>
        <div class=${"bar-fill " + level} style=${"width:" + (share * 100).toFixed(2) + "%"} />
      </div>
      <div class="fact dim">
        <span>${compact(used)} used</span>
        <span>${compact(limit)} limit</span>
      </div>
    </div>
  `;
}

// Everything this panel reads off the transcript, in one pass instead of
// three — one of which used to copy the whole array to reverse it, on
// every frame of a streaming turn.
function digest(events) {
  let meta = null;
  let latest = null;
  let compactions = 0;
  for (const event of events) {
    if (event.type === "assistant_message") latest = event;
    else if (event.type === "compaction") compactions += 1;
    else if (event.type === "meta" && !meta) meta = event;
  }
  return { meta: meta || {}, latest, compactions };
}

function DetailPanel({ id, view }) {
  const [children, setChildren] = useState([]);
  useEffect(() => {
    setChildren([]);
    if (!id) return undefined;
    let alive = true;
    const refresh = () =>
      api(apiPath(id, "/children"))
        .then((page) => alive && setChildren(page.children || []))
        // One dropped poll is not "no subagents": keep the last list we
        // were told about rather than blanking the panel every time a
        // five-second request loses.
        .catch(() => {});
    refresh();
    const timer = setInterval(refresh, 5000);
    return () => {
      alive = false;
      clearInterval(timer);
    };
  }, [id]);

  const session = view.session || {};
  // The array is mutated in place — appended to, truncated by a rewind —
  // so its identity alone would not tell a stale digest from a fresh
  // one; its length moves for both.
  const { meta, latest, compactions } = useMemo(
    () => digest(view.events),
    [view.events, view.events.length, view.count],
  );

  if (!id) return html`<aside class="panel"></aside>`;
  // The newest turn's own usage, which is what the model was holding —
  // not the session total, which counts every turn's context again.
  const used = contextTokens(latest && latest.usage);
  const running = children.filter((child) => child.state !== "idle").length;

  return html`
    <aside class="panel">
      <${Card} title="Session">
        <${ContextBar} used=${used} limit=${session.context_limit} />
        <div class="fact"><span>Model</span><span class="mono">${session.model || meta.model || "—"}</span></div>
        <div class="fact"><span>Agent</span><span class="mono">${session.agent || meta.agent || "—"}</span></div>
        <div class="fact">
          <span>Directory</span>
          <span class="mono ellipsis" title=${session.cwd || meta.cwd || ""}>
            ${basename(session.cwd || meta.cwd) || "—"}
          </span>
        </div>
        ${compactions > 0 &&
        html`<div class="fact"><span>Compactions</span><span>${compactions}</span></div>`}
      <//>
      <${Card} title="Subagents" extra=${children.length ? running + "/" + children.length : "0"}>
        ${!children.length && html`<p class="note">none</p>`}
        ${children.map(
          (child) => html`
            <a class="subagent" key=${child.id} href=${"#/s/" + encodeURIComponent(child.id)}>
              <span class="subagent-name">${preview(child.title || child.agent || child.id, 44)}</span>
              <span class=${"subagent-state " + (child.state || "idle")}>
                ${child.state === "working" ? "is working" : child.state === "stalled" ? "stalled" : "done"}
              </span>
            </a>
          `,
        )}
      <//>
    </aside>
  `;
}

function Center({ id, view, session, onDrawer }) {
  const stream = useRef(null);
  const follow = useRef(true);
  const [tailing, setTailing] = useState(true);

  const toBottom = useCallback(() => {
    const node = stream.current;
    if (node) node.scrollTop = node.scrollHeight;
    follow.current = true;
    setTailing(true);
  }, []);

  // After every paint: the tail stays pinned unless someone scrolled up.
  useEffect(() => {
    if (follow.current && stream.current) stream.current.scrollTop = stream.current.scrollHeight;
  });
  useEffect(() => {
    follow.current = true;
    setTailing(true);
  }, [id]);

  const onScroll = () => {
    const node = stream.current;
    if (!node) return;
    const bottom = node.scrollHeight - node.scrollTop - node.clientHeight < 80;
    follow.current = bottom;
    if (bottom !== tailing) setTailing(bottom);
  };

  const head = view.session || session || {};
  const title = head.title || id || "";
  const rows = id ? pageRows(view.events, view.base, id, head.cwd) : [];

  return html`
    <main class="center">
      <header class="center-head">
        <button class="drawer-toggle" type="button" onClick=${onDrawer} title="sessions">☰</button>
        <h1>${id ? inline(preview(title, 120)) : "ilar"}</h1>
      </header>
      ${view.error &&
      html`
        <p class="banner">
          <span>${view.error}</span>
          ${view.retryable &&
          view.retry &&
          html`<button class="retry" type="button" onClick=${() => view.retry()}>retry</button>`}
        </p>
      `}
      <div class="stream" ref=${stream} onScroll=${onScroll}>
        <div class="stream-inner">
          ${!id && html`<p class="empty">Pick a session on the left.</p>`}
          ${id && view.loading && html`<p class="empty">loading…</p>`}
          ${view.hasMore &&
          html`
            <div class="earlier">
              <button type="button" disabled=${view.pending} onClick=${() => view.earlier && view.earlier()}>
                ${view.pending ? "loading…" : "load earlier"}
              </button>
            </div>
          `}
          ${rows}
          <${LiveStep} live=${view.live} />
          ${view.rewound !== null &&
          html`<div class="divider">rewound ${view.rewound} events</div>`}
        </div>
      </div>
      <footer class="composer">
        ${!tailing && html`<button class="jump" type="button" onClick=${toBottom}>jump to live ↓</button>`}
        ${id &&
        html`<${Composer} key=${id} id=${id} view=${view} session=${session} />`}
      </footer>
    </main>
  `;
}

// ------------------------------------------------------------- routing

function routeId() {
  const hash = location.hash.replace(/^#/, "");
  return hash.startsWith("/s/") ? decodeURIComponent(hash.slice(3)) : null;
}

function App() {
  const [id, setId] = useState(routeId());
  const [sessions, setSessions] = useState([]);
  const [error, setError] = useState("");
  const [drawer, setDrawer] = useState(false);
  // One fetch, one stream, two readers: the centre pane renders the
  // transcript and the detail panel reads its usage and its compactions
  // off the same array rather than asking for the page again.
  const view = useTranscript(id);

  useEffect(() => {
    const onHash = () => {
      setId(routeId());
      setDrawer(false);
    };
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);

  // The listing is a poll on the server too; a slow one here keeps the
  // live dots honest without holding a socket open per tab.
  useEffect(() => {
    let alive = true;
    const refresh = () =>
      api("/api/sessions")
        .then((listing) => {
          if (!alive) return;
          setSessions(listing.sessions || []);
          setError("");
        })
        .catch((failure) => alive && setError(message(failure)));
    refresh();
    const timer = setInterval(refresh, 3000);
    return () => {
      alive = false;
      clearInterval(timer);
    };
  }, []);

  const session = sessions.find((row) => row.id === id) || null;
  return html`
    <div class=${"app" + (drawer ? " drawer" : "")}>
      <${Sidebar}
        sessions=${sessions}
        current=${id}
        error=${error}
        onPick=${() => setDrawer(false)}
        onCreated=${(created) => {
          // The hash is the router: selecting the new session is the
          // same gesture as clicking it in the list.
          location.hash = "#/s/" + encodeURIComponent(created);
          setDrawer(false);
        }}
      />
      <${Center} id=${id} view=${view} session=${session} onDrawer=${() => setDrawer(!drawer)} />
      <${DetailPanel} id=${id} view=${view} />
    </div>
  `;
}

render(html`<${App} />`, document.getElementById("app"));
