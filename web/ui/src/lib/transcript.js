// One open session: the canonical folded events from `base` on, the log
// line the tail stands at (where the stream resumes), and the running
// step's scratch.
//
// The state is a mutable object, not reactive state: the fold is index
// arithmetic over an array a rewind truncates in place, and copying it
// per frame would make a busy turn quadratic. `paint` (rAF-coalesced by
// the caller) asks the UI to look again.

import { liveApply } from "./rows.js";

const message = (error) => String((error && error.message) || error);

// The waits before each reconnect; the list ends, so the page stops
// claiming the stream is coming back.
const RECONNECT_WAITS = [300, 800, 1500, 3000];
const CLOSED = 2; // EventSource.CLOSED

/** Folds one line-bearing frame. A frame at or below the line we stand
 * on was folded already (a resumed stream replays): "stale". A rewind
 * below the loaded window: "gone". */
export function fold(view, kind, data) {
  if (typeof data.line !== "number" || data.line <= view.line) return "stale";
  if (kind === "rewind") {
    if (data.to < view.base) return "gone";
    // Clamped: this client may hold less than the rewind cuts to.
    const kept = Math.min(view.events.length, data.to - view.base);
    view.rewound = Math.max(0, view.events.length - kept);
    view.events.length = kept;
  } else {
    view.rewound = null;
    view.events.push(data.event);
  }
  view.line = data.line;
  return "applied";
}

export function blankView(id, shared) {
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
    status: shared ? "shared transcript" : id ? "loading…" : "",
    error: "",
    // Whether a button can do anything about the error (a deleted
    // session it cannot).
    retryable: false,
    loading: Boolean(id),
    pending: false,
    retry: () => {},
    earlier: () => {},
    close: () => {},
  };
}

/** Opens `id` on `source` ({api, openStream, SHARE}); returns the view. */
export function openTranscript(id, source, paint) {
  const view = blankView(id, source.SHARE);
  if (!id) return view;
  const apiPath = (suffix) => "/api/sessions/" + encodeURIComponent(id) + (suffix || "");

  let alive = true;
  let stream = null;
  let waiting = null;
  let attempts = 0;
  // Which load the view describes: a reply from an older one is dropped.
  let generation = 0;

  const detach = () => {
    if (stream) stream.close();
    stream = null;
    if (waiting) clearTimeout(waiting);
    waiting = null;
  };

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
    // Deltas are not resumed, so the half-message would gain a hole.
    view.live = null;
    view.status = "reconnecting…";
    paint();
    waiting = setTimeout(() => {
      waiting = null;
      if (alive) attach();
    }, wait);
  };

  // Our own reconnect, not the browser's: an EventSource retries the URL
  // it was built with, `?from=` the line it first attached at.
  const attach = () => {
    detach();
    const mine = source.openStream(id, view.line);
    if (!mine) return;
    stream = mine;
    const current = () => alive && stream === mine;
    const on = (name, handler) =>
      mine.addEventListener(name, (frame) => {
        if (current()) handler(JSON.parse(frame.data));
      });
    mine.addEventListener("open", () => {
      if (!current()) return;
      attempts = 0;
      view.status = "live";
      view.error = "";
      view.retryable = false;
      paint();
    });
    // The server's `error` frame carries data; a transport failure none.
    mine.addEventListener("error", (frame) => {
      if (!current()) return;
      if (frame && typeof frame.data === "string") {
        const failure = JSON.parse(frame.data);
        // A failed turn is news about the session, not the stream.
        if (failure.scope === "turn") {
          view.error = failure.message;
          view.retryable = false;
          paint();
          return;
        }
        detach();
        view.status = "stopped";
        view.error = failure.message;
        view.retryable = true;
        paint();
      } else if (mine.readyState === CLOSED) {
        offline("The live stream was refused — reload or retry.");
      } else if (attempts >= RECONNECT_WAITS.length) {
        offline("Lost the live stream to this session — the server may be gone.");
      } else {
        reconnect();
      }
    });
    on("append", (data) => {
      if (fold(view, "append", data) !== "applied") return;
      // The committed step replaces its stand-in; a tool result does not
      // (the step's other tools may still stream).
      if (data.event && data.event.type === "assistant_message") view.live = null;
      view.count += 1;
      paint();
    });
    on("delta", (data) => {
      // A new turn supersedes "the last one failed".
      if (data.type === "turn_started" && !view.retryable) view.error = "";
      view.live = liveApply(view.live, data);
      paint();
    });
    on("rewind", (data) => {
      const outcome = fold(view, "rewind", data);
      if (outcome === "gone") reload();
      else if (outcome === "applied") paint();
    });
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
      const page = await source.api(apiPath());
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
      const page = await source.api(apiPath("?from=" + from));
      // A reload landed meanwhile: this head no longer sits above it.
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
  view.close = () => {
    alive = false;
    detach();
  };

  load();
  return view;
}
