// The page, by hand: hash routing, one EventSource per open session,
// and the two-line fold over append/rewind.
//
// One rule runs through all of it: no string that came from the server
// is ever handed to an HTML parser. Every value goes in through
// textContent or createElement, so a session that contains "<script>"
// renders those characters and nothing happens. There is no markdown
// library and no innerHTML below this line.

"use strict";

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

async function api(path) {
  const headers = token ? { Authorization: "Bearer " + token } : {};
  const response = await fetch(path, { headers });
  if (!response.ok) throw new Error("GET " + path + " → " + response.status);
  return response.json();
}

// --------------------------------------------------------------- dom

const $ = (id) => document.getElementById(id);

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined && text !== null) node.textContent = String(text);
  return node;
}

function clear(node) {
  while (node.firstChild) node.removeChild(node.firstChild);
  return node;
}

function banner(message) {
  const node = $("banner");
  node.textContent = message || "";
  node.hidden = !message;
}

function relative(iso) {
  const then = Date.parse(iso);
  if (Number.isNaN(then)) return "";
  const seconds = Math.max(0, (Date.now() - then) / 1000);
  if (seconds < 60) return Math.floor(seconds) + "s ago";
  if (seconds < 3600) return Math.floor(seconds / 60) + "m ago";
  if (seconds < 86400) return Math.floor(seconds / 3600) + "h ago";
  if (seconds < 86400 * 30) return Math.floor(seconds / 86400) + "d ago";
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

// ------------------------------------------------------- text render

// Plain text first: fenced blocks, inline backticks, bare http(s) links
// and diff colouring. Everything else is left exactly as written —
// asterisks stay asterisks, because guessing at markdown in a shell
// transcript is how a path turns into italics.
const INLINE = /`([^`\n]+)`|(https?:\/\/[^\s<>"'`)\]]+)/g;
const TRAILING = /[.,;:!?]+$/;

function inlineText(text) {
  const node = el("div", "text");
  let last = 0;
  for (const match of String(text).matchAll(INLINE)) {
    if (match.index > last) node.appendChild(document.createTextNode(text.slice(last, match.index)));
    if (match[1] !== undefined) {
      node.appendChild(el("code", "inline", match[1]));
    } else {
      const url = match[2].replace(TRAILING, "");
      const link = el("a", "link", url);
      // The regex admits http(s) only, so no javascript: URL can reach
      // an href here.
      link.href = url;
      link.target = "_blank";
      link.rel = "noreferrer noopener";
      node.appendChild(link);
      node.appendChild(document.createTextNode(match[2].slice(url.length)));
    }
    last = match.index + match[0].length;
  }
  node.appendChild(document.createTextNode(text.slice(last)));
  return node;
}

function codeBlock(body) {
  const pre = el("pre", "code");
  pre.appendChild(el("code", null, body));
  return pre;
}

function renderText(host, text) {
  const lines = String(text === null || text === undefined ? "" : text).split("\n");
  let buffer = [];
  let fenced = null;
  const flush = () => {
    if (buffer.join("\n").trim()) host.appendChild(inlineText(buffer.join("\n")));
    buffer = [];
  };
  for (const line of lines) {
    const fence = /^\s*```/.test(line);
    if (fenced !== null) {
      if (fence) {
        host.appendChild(codeBlock(fenced.join("\n")));
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
  if (fenced !== null) host.appendChild(codeBlock(fenced.join("\n")));
  flush();
  return host;
}

function looksLikeDiff(text) {
  if (/^(@@ |diff --git |--- |\+\+\+ )/m.test(text)) return true;
  const lines = text.split("\n");
  const added = lines.filter((line) => /^\+(?!\+)/.test(line)).length;
  const removed = lines.filter((line) => /^-(?!-)/.test(line)).length;
  return added > 0 && removed > 0 && added + removed >= lines.length * 0.3;
}

function diffBlock(text) {
  const pre = el("pre", "diff");
  for (const line of text.split("\n")) {
    let kind = "ctx";
    if (line.startsWith("@@")) kind = "hunk";
    else if (/^\+(?!\+\+)/.test(line)) kind = "add";
    else if (/^-(?!--)/.test(line)) kind = "del";
    pre.appendChild(el("span", kind, line + "\n"));
  }
  return pre;
}

// A tool's detail or result: a diff if it reads like one, plain
// pre-wrap otherwise. Never markdown — this is program output.
function preformatted(text, className) {
  return looksLikeDiff(text) ? diffBlock(text) : el("pre", className || "detail", text);
}

// A collapsed row that opens on click. The label is built by the
// caller; nothing here interprets it.
function disclosure(host, label, expanded, fill) {
  const button = el("button", "disclosure");
  button.type = "button";
  const caret = el("span", "badge", expanded ? "▾" : "▸");
  button.appendChild(caret);
  button.appendChild(label);
  const body = el("div", "body");
  body.hidden = !expanded;
  let filled = false;
  const open = () => {
    if (!filled) {
      filled = true;
      fill(body);
    }
  };
  if (expanded) open();
  button.addEventListener("click", () => {
    body.hidden = !body.hidden;
    caret.textContent = body.hidden ? "▸" : "▾";
    if (!body.hidden) open();
  });
  host.appendChild(button);
  host.appendChild(body);
  return body;
}

// -------------------------------------------------------------- rows

function images(host, sessionId, eventId, descriptors) {
  for (const image of descriptors || []) {
    const thumb = el("img", "thumb");
    thumb.loading = "lazy";
    thumb.alt = image.media_type + " · " + tokens(image.bytes) + " bytes";
    const at = "/images/" + encodeURIComponent(eventId) + "/" + image.n;
    thumb.src = withToken(apiPath(sessionId, at));
    host.appendChild(thumb);
  }
}

// The first line of something long, for a row that is still collapsed.
function preview(text) {
  const line = String(text || "").split("\n").find((one) => one.trim()) || "";
  return line.length > 90 ? line.slice(0, 89) + "…" : line;
}

function row(host, className, who) {
  const node = el("div", "row " + className);
  if (who) node.appendChild(el("div", "who", who));
  host.appendChild(node);
  return node;
}

function toolLabel(call, result) {
  const state = !result ? "run" : result.is_error ? "err" : "ok";
  const mark = !result ? "…" : result.is_error ? "!" : "✓";
  const label = el("span");
  label.appendChild(el("span", "badge " + state, mark));
  label.appendChild(el("span", "tool-name", " " + call.name + " "));
  if (call.agent && call.agent.name) {
    label.appendChild(el("span", "tag", call.agent.name + " "));
  }
  label.appendChild(el("span", "summary", call.summary || ""));
  return label;
}

function toolBody(body, sessionId, call, result) {
  if (call.detail) body.appendChild(preformatted(call.detail));
  if (!result) {
    body.appendChild(el("p", "note", "running…"));
    return;
  }
  if (result.text) body.appendChild(preformatted(result.text, result.is_error ? "detail error" : "detail"));
  images(body, sessionId, result.id, result.images);
  if (result.truncated) {
    const at = "/results/" + encodeURIComponent(result.tool_use_id);
    const link = el("a", "link", "full output");
    link.href = withToken(apiPath(sessionId, at));
    link.target = "_blank";
    link.rel = "noreferrer noopener";
    body.appendChild(link);
  }
}

// A task row's detail is a whole child transcript, fetched only when
// someone opens it — the parent log holds a link, never the turns.
function taskBody(body, call, result) {
  const child = result && result.child_session_id;
  if (!child) {
    body.appendChild(el("p", "note", result ? "no child session recorded" : "running…"));
    return;
  }
  if (call.detail) body.appendChild(preformatted(call.detail));
  const nested = el("div", "child");
  body.appendChild(nested);
  nested.appendChild(el("p", "note", "loading…"));
  api(apiPath(child, "?invocation=" + encodeURIComponent(call.id)))
    .then((page) => {
      clear(nested);
      renderEvents(nested, page.events.slice(compactionCut(page.events, page.cursor)), child);
      const link = el("a", "link", "open the child session");
      link.href = "#/s/" + encodeURIComponent(child);
      nested.appendChild(link);
    })
    .catch((error) => {
      clear(nested).appendChild(el("p", "note", String(error.message || error)));
    });
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

function renderEvents(host, events, sessionId) {
  const results = new Map();
  for (const event of events) {
    if (event.type === "tool_result") results.set(event.tool_use_id, event);
  }
  const consumed = new Set();
  for (const event of events) {
    switch (event.type) {
      case "user_message": {
        const node = row(host, "user", "you");
        renderText(node, event.text);
        images(node, sessionId, event.id, event.images);
        break;
      }
      case "assistant_message":
        for (const block of event.content || []) {
          if (block.type === "text") {
            renderText(row(host, "assistant", event.model || "assistant"), block.text);
          } else if (block.type === "reasoning_summary") {
            const label = el("span", "tag", " thinking · " + preview(block.text));
            disclosure(row(host, "thought"), label, false, (body) => renderText(body, block.text));
          } else if (block.type === "tool_call") {
            const result = results.get(block.id);
            if (result) consumed.add(result.id);
            const fill = (body) =>
              block.name === "task"
                ? taskBody(body, block, result)
                : toolBody(body, sessionId, block, result);
            disclosure(row(host, "tool"), toolLabel(block, result), false, fill);
          }
        }
        break;
      case "tool_result": {
        // Only a result whose call is off the top of this page: the rest
        // render inside the call they answer.
        if (consumed.has(event.id)) break;
        const node = row(host, event.is_error ? "tool error" : "tool", "result");
        if (event.text) node.appendChild(preformatted(event.text));
        images(node, sessionId, event.id, event.images);
        break;
      }
      case "model_change":
        row(host, "system", "model → " + event.model + (event.variant ? " · " + event.variant : ""));
        break;
      case "compaction": {
        const label = el("span", "tag", " transcript compacted");
        disclosure(row(host, "system"), label, false, (body) => renderText(body, event.summary));
        break;
      }
      case "rewind":
        host.appendChild(el("div", "divider", "rewound to event " + event.to));
        break;
      default:
        // meta, checkpoint, topic, subagent_invocation: state, not
        // transcript. They keep their index and render nothing.
        break;
    }
  }
  return host;
}

// -------------------------------------------------------------- list

function group(sessions) {
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
  // Newest group first, with the sessions that never named a directory
  // last: they have nothing to group by, not the newest nothing.
  ordered.sort((left, right) => (left.cwd === "") - (right.cwd === "") || right.newest - left.newest);
  return ordered;
}

function sessionRow(session) {
  const link = el("a", "session-row");
  link.href = "#/s/" + encodeURIComponent(session.id);
  link.appendChild(el("span", "dot" + (session.live ? " on" : "")));
  link.appendChild(el("span", "row-title", session.title || session.id));
  if (session.agent) link.appendChild(el("span", "tag", session.agent));
  if (session.model) link.appendChild(el("span", "tag", session.model));
  link.appendChild(el("span", "tag", relative(session.modified)));
  return link;
}

async function renderList() {
  const listing = await api("/api/sessions");
  if (view.route !== "list") return;
  const sessions = listing.sessions || [];
  const host = clear($("groups"));
  for (const { cwd, rows } of group(sessions)) {
    const section = el("div", "group");
    section.appendChild(el("div", "group-cwd", cwd || "elsewhere"));
    for (const session of rows) section.appendChild(sessionRow(session));
    host.appendChild(section);
  }
  $("list-empty").hidden = sessions.length > 0;
  const live = sessions.filter((session) => session.live).length;
  status(sessions.length + " sessions · " + live + " live", live > 0);
}

// ----------------------------------------------------------- session

// One open session. `events` is the canonical folded stream from
// `base` onward; `line` is the physical line the tail stands at, which
// is what the SSE stream resumes from.
const view = {
  route: "list",
  id: null,
  epoch: 0,
  events: [],
  base: 0,
  hasMore: false,
  line: 0,
  source: null,
  follow: true,
  pending: false,
  rewound: null,
};

function status(text, live) {
  const node = $("status");
  node.textContent = text;
  node.className = "status" + (live ? " live" : "");
}

function detach() {
  if (view.source) {
    view.source.close();
    view.source = null;
  }
}

// The design's fold, over the canonical array, with the one adjustment
// paging forces: index 0 of what we hold is canonical index `base`.
function fold(kind, data) {
  if (kind === "rewind") {
    if (data.to < view.base) return false;
    view.rewound = view.base + view.events.length - data.to;
    view.events.length = data.to - view.base;
  } else {
    view.rewound = null;
    view.events.push(data.event);
  }
  view.line = data.line;
  return true;
}

let scheduled = false;
function scheduleRender() {
  if (scheduled) return;
  scheduled = true;
  requestAnimationFrame(() => {
    scheduled = false;
    if (view.route === "session") renderTranscript();
  });
}

function renderTranscript() {
  const host = clear($("transcript"));
  renderEvents(host, view.events.slice(compactionCut(view.events, view.base)), view.id);
  if (view.rewound !== null) {
    host.appendChild(el("div", "divider", "rewound " + view.rewound + " events"));
  }
  $("earlier").hidden = !view.hasMore;
  if (view.follow) window.scrollTo(0, document.body.scrollHeight);
}

function renderHead(page) {
  const session = page.session || {};
  const meta = view.events.find((event) => event.type === "meta") || {};
  const host = clear($("session-head"));
  host.appendChild(el("h1", null, session.title || view.id));
  const facts = el("div", "facts");
  const fact = (text) => text && facts.appendChild(el("span", null, text));
  fact(session.model || meta.model);
  fact(session.agent || meta.agent);
  fact(session.cwd || meta.cwd);
  const usage = page.usage || {};
  fact("in " + tokens(usage.input) + " · out " + tokens(usage.output));
  fact("cache " + tokens(usage.cache_read) + "/" + tokens(usage.cache_creation));
  if (typeof usage.cost_dollars === "number") fact(cost(usage.cost_dollars));
  else if (usage.plan) fact("plan");
  fact(page.count + " events");
  host.appendChild(facts);
  clear($("crumb")).appendChild(document.createTextNode(session.title || view.id));
}

function attach() {
  detach();
  const source = new EventSource(withToken(apiPath(view.id, "/events?from=" + view.line)));
  view.source = source;
  const epoch = view.epoch;
  const guard = (handler) => (message) => {
    if (view.epoch !== epoch) return;
    handler(JSON.parse(message.data));
  };
  source.addEventListener("open", () => {
    if (view.epoch === epoch) status("live", true);
  });
  // The server's terminal `error` frame and `EventSource`'s own
  // transport failure arrive under the same name; only the server's
  // carries data. A dropped socket is not a reason to tear the page
  // down — EventSource reconnects on its own, with `Last-Event-ID`.
  source.addEventListener("error", (message) => {
    if (view.epoch !== epoch) return;
    if (message && typeof message.data === "string") {
      detach();
      status("stopped", false);
      banner(JSON.parse(message.data).message);
    } else {
      status("reconnecting…", false);
    }
  });
  const on = (name, handler) => source.addEventListener(name, guard(handler));
  on("append", (data) => {
    fold("append", data);
    scheduleRender();
  });
  on("rewind", (data) => {
    // A rewind below the loaded window leaves nothing to fold onto.
    if (fold("rewind", data)) scheduleRender();
    else openSession(view.id);
  });
  // The view is stale — a lagging subscriber or a repaired tail. Only a
  // re-fetch is honest.
  on("resync", () => openSession(view.id));
  on("deleted", () => {
    detach();
    status("deleted", false);
    banner("This session was deleted.");
  });
}

async function loadEarlier() {
  if (view.pending || !view.hasMore) return;
  view.pending = true;
  const epoch = view.epoch;
  const before = document.body.scrollHeight;
  try {
    const page = await api(apiPath(view.id, "?from=" + view.base));
    if (view.epoch !== epoch) return;
    view.events = page.events.concat(view.events);
    view.base = page.cursor;
    view.hasMore = page.has_more;
    view.follow = false;
    renderTranscript();
    window.scrollBy(0, document.body.scrollHeight - before);
  } catch (error) {
    banner(String(error.message || error));
  } finally {
    view.pending = false;
  }
}

async function openSession(id) {
  detach();
  view.epoch += 1;
  const epoch = view.epoch;
  view.route = "session";
  view.id = id;
  view.follow = true;
  view.rewound = null;
  banner("");
  status("loading…", false);
  show("session-view");
  try {
    const page = await api(apiPath(id));
    if (view.epoch !== epoch) return;
    view.events = page.events || [];
    view.base = page.cursor;
    view.hasMore = page.has_more;
    view.line = page.line;
    renderHead(page);
    renderTranscript();
    attach();
  } catch (error) {
    if (view.epoch !== epoch) return;
    clear($("transcript"));
    status("", false);
    banner(String(error.message || error));
  }
}

// ------------------------------------------------------ routing/boot

function show(id) {
  for (const section of document.querySelectorAll(".view")) {
    section.hidden = section.id !== id;
  }
}

let listTimer = null;

function navigate() {
  const hash = location.hash.replace(/^#/, "");
  detach();
  if (listTimer) {
    clearInterval(listTimer);
    listTimer = null;
  }
  if (hash.startsWith("/s/")) {
    openSession(decodeURIComponent(hash.slice(3)));
    return;
  }
  view.epoch += 1;
  view.route = "list";
  view.id = null;
  banner("");
  clear($("crumb"));
  show("list-view");
  const refresh = () =>
    renderList().catch((error) => banner(String(error.message || error)));
  refresh();
  // The listing is a poll on the server too; a slow one here keeps the
  // live dots honest without holding a socket open per tab.
  listTimer = setInterval(refresh, 3000);
}

window.addEventListener("hashchange", navigate);
window.addEventListener("scroll", () => {
  const bottom = window.innerHeight + window.scrollY >= document.body.scrollHeight - 80;
  view.follow = bottom;
});
$("earlier").addEventListener("click", loadEarlier);
navigate();
