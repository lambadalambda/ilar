// The phone's side of `/remote`. Pairs with one TUI session over iroh
// (the WebAssembly client in ./pkg), keeps this device's key so the
// session knows it again, and hands app.js — the renderer `ilar serve`
// and share files use — a source that reads from the wire.
//
// The wire is crates/ilar-remote/src/wire.rs: a `snapshot` (the whole log
// as one page in serve's shape), then `append`, `live`, `busy`, `prompt`
// and `notice` frames; this page sends `prompt`, `grant`, `question` and
// `cancel`.

import init, { connect } from "./pkg/ilar_remote_web.js";

const STORE = "ilar.remote";
const CONNECT_TIMEOUT_MS = 20000;
const NOTICE_MS = 6000;

// ---------------------------------------------------------- storage

// The device key must survive a reload: pairing pins it, and a new key
// is refused for good. Kept with the link, so a reopened app reconnects
// without the code.
function stored() {
  try {
    return JSON.parse(localStorage.getItem(STORE) || "null");
  } catch (error) {
    return null;
  }
}

function store(value) {
  try {
    localStorage.setItem(STORE, JSON.stringify(value));
  } catch (error) {
    // Private mode: this visit works, the next one pairs again.
  }
}

// A scanned code opens this page with the ticket after `#`. Taken once
// and removed from the address bar, so it does not linger in history.
function takeLink() {
  const fragment = location.hash.slice(1);
  if (!fragment || fragment.startsWith("/")) return null;
  history.replaceState(null, "", location.pathname + location.search + "#/");
  return location.origin + location.pathname + "#" + fragment;
}

// --------------------------------------------------------- the wire

let link = null;
let key = [];
let remote = null; // the wasm Remote while connected
let connecting = null;

let page = null;
let live = [];
let busy = false;
let prompt = null;
let notice = "";
let noticeTimer = null;
// Snapshots received, and where the page app.js last loaded stood: a
// stream that opens behind the page (a new snapshot, or appends since)
// tells the renderer to reload rather than miss them.
let snapshots = 0;
let served = { snapshots: 0, line: 0 };

const listeners = new Set();
const changed = () => listeners.forEach((listener) => listener());

let active = null; // the stream app.js is listening to

// The three methods app.js uses of an EventSource. Frames arrive as
// `{data: json}` like an EventSource's; a transport failure carries no
// data, which app.js reads as "reconnect".
class WireStream {
  constructor() {
    this.readyState = 0;
    this.handlers = new Map();
  }
  addEventListener(name, handler) {
    if (!this.handlers.has(name)) this.handlers.set(name, []);
    this.handlers.get(name).push(handler);
  }
  emit(name, data) {
    const frame = data === undefined ? {} : { data: JSON.stringify(data) };
    for (const handler of this.handlers.get(name) || []) handler(frame);
  }
  close() {
    this.readyState = 2;
    if (active === this) active = null;
  }
}

// Frames reach an open stream only. The page and the live step are kept
// up to date here as well, so a renderer that attaches later (first
// load, after a resync) starts from what the wire has said, not from the
// last snapshot.
const open = () => active && active.readyState === 1;

function handle(message) {
  switch (message.type) {
    case "snapshot":
      page = message.page;
      live = message.live || [];
      busy = message.busy;
      prompt = message.prompt;
      snapshots += 1;
      if (open()) active.emit("resync", {});
      break;
    case "append":
      page.events.push(message.event);
      page.line = message.line;
      page.count += 1;
      // The committed step replaces its stand-in, as app.js does.
      if (message.event && message.event.type === "assistant_message") live = [];
      if (open()) active.emit("append", { line: message.line, event: message.event });
      break;
    case "live":
      if (message.delta.type === "reset") live = [];
      else live.push(message.delta);
      if (open()) active.emit("delta", message.delta);
      break;
    case "busy":
      busy = message.busy;
      break;
    case "prompt":
      prompt = message.prompt;
      break;
    case "notice":
      notice = message.text;
      clearTimeout(noticeTimer);
      noticeTimer = setTimeout(() => {
        notice = "";
        changed();
      }, NOTICE_MS);
      break;
  }
  changed();
}

async function pump(connection) {
  try {
    for (;;) {
      const text = await connection.next();
      if (text === null) break;
      handle(JSON.parse(text));
    }
  } catch (error) {
    console.warn("remote: the session's stream ended", error);
  }
  if (remote === connection) remote = null;
  connection.close();
  connection.free();
  if (active) active.emit("error");
}

const kind = (error) => String((error && error.message) || error).split(":")[0];

function withTimeout(promise, ms) {
  return Promise.race([
    promise,
    new Promise((_, reject) => setTimeout(() => reject(new Error("network: no answer in time")), ms)),
  ]);
}

// One dial at a time; resolves once the first snapshot is in.
function ensureConnected() {
  if (remote) return Promise.resolve();
  if (connecting) return connecting;
  connecting = (async () => {
    const before = snapshots;
    // A dial that answers after its timeout is closed, not kept: the
    // session lets the newest connection win, and a late orphan would
    // kick the one this page is using.
    let late = false;
    const dial = connect(link, new Uint8Array(key)).then((connection) => {
      if (!late) return connection;
      connection.close();
      connection.free();
      throw new Error("network: answered too late");
    });
    const connection = await withTimeout(dial, CONNECT_TIMEOUT_MS).catch((error) => {
      late = true;
      throw error;
    });
    key = Array.from(connection.key());
    store({ link, key });
    remote = connection;
    pump(connection);
    // The snapshot is the first frame; app.js needs it before it can
    // render anything.
    let check = null;
    await withTimeout(
      new Promise((resolve) => {
        check = () => snapshots > before && resolve();
        listeners.add(check);
        check();
      }),
      CONNECT_TIMEOUT_MS,
    ).finally(() => listeners.delete(check));
  })().finally(() => {
    connecting = null;
  });
  return connecting;
}

function send(command) {
  if (!remote) return Promise.reject(new Error("not connected to the session"));
  return remote.send(JSON.stringify(command));
}

// ---------------------------------------------------- app.js's source

const sessionRow = () => ({ ...(page && page.session), state: busy ? "working" : "idle", driven: true });

window.__ILAR_REMOTE__ = {
  get id() {
    return page && page.id;
  },
  api(path) {
    const base = "/api/sessions/" + encodeURIComponent(page.id);
    if (path === "/api/sessions") return Promise.resolve({ sessions: [sessionRow()] });
    if (path === base) {
      served = { snapshots, line: page.line };
      // A copy: app.js folds appends into the array it is given, and so
      // does handle() into this one.
      return Promise.resolve({ ...page, events: page.events.slice(), session: sessionRow() });
    }
    if (path === base + "/children") return Promise.resolve({ children: [] });
    return Promise.reject(new Error("the full text is not sent to the phone"));
  },
  async post(path, body) {
    if (path.endsWith("/message")) {
      await send({ type: "prompt", text: body.text });
      return { fate: "sent" };
    }
    if (path.endsWith("/abort")) {
      await send({ type: "cancel" });
      return { fate: "cancelling" };
    }
    throw new Error("not available over remote: " + path);
  },
  stream() {
    const stream = new WireStream();
    active = stream;
    ensureConnected().then(
      () => {
        if (active !== stream) return;
        stream.readyState = 1;
        stream.emit("open");
        if (snapshots > served.snapshots || page.line > served.line) stream.emit("resync", {});
        else live.forEach((delta) => stream.emit("delta", delta));
      },
      (error) => {
        if (active !== stream) return;
        if (kind(error) === "refused") return unpair("This device was refused. Scan a new code.");
        stream.emit("error");
      },
    );
    return stream;
  },
  state: () => ({ prompt, notice }),
  subscribe(listener) {
    listeners.add(listener);
    return () => listeners.delete(listener);
  },
  send,
};

// ----------------------------------------------------------- pairing

const $ = (id) => document.getElementById(id);

function showPairing(error) {
  $("app").hidden = true;
  $("pair").hidden = false;
  $("pair-error").hidden = !error;
  $("pair-error").textContent = error || "";
  $("pair-status").textContent = "";
  $("pair-scan").hidden = !("BarcodeDetector" in window && navigator.mediaDevices);
}

// The session will not take this device again: forget its link (the
// key stays, it is this device's) and start over with the reason on
// screen. A reload, because app.js has already rendered the old session.
function unpair(why) {
  store({ key });
  try {
    sessionStorage.setItem(STORE + ".why", why);
  } catch (error) {
    // Without storage the page reloads to the plain pairing screen.
  }
  location.reload();
}

async function start(newLink) {
  link = newLink;
  $("pair").hidden = false;
  $("pair-error").hidden = true;
  $("pair-status").textContent = "connecting…";
  try {
    await ensureConnected();
  } catch (error) {
    const reason = {
      refused: "This device was refused: the code was used by another device, or it belongs to a session that has stopped remote control.",
      ticket: "That is not an ilar remote link.",
      busy: "The session is busy; try again in a moment.",
    }[kind(error)];
    showPairing(
      reason ||
        "Could not reach the session. If its remote control has ended, scan a new code. (" +
          String(error.message || error) +
          ")",
    );
    if (kind(error) === "refused" || kind(error) === "ticket") store({ key });
    return;
  }
  $("pair").hidden = true;
  $("app").hidden = false;
  await import("./app.js");
}

$("pair-go").addEventListener("click", () => {
  const value = $("pair-link").value.trim();
  if (value) start(value);
});

// Chrome on Android has a detector; elsewhere the system camera opens
// the code's link, which lands here with the ticket.
$("pair-scan").addEventListener("click", async () => {
  const video = $("pair-video");
  try {
    const camera = await navigator.mediaDevices.getUserMedia({ video: { facingMode: "environment" } });
    video.srcObject = camera;
    video.hidden = false;
    await video.play();
    const detector = new window.BarcodeDetector({ formats: ["qr_code"] });
    const look = async () => {
      const codes = await detector.detect(video).catch(() => []);
      if (codes.length) {
        camera.getTracks().forEach((track) => track.stop());
        video.hidden = true;
        start(codes[0].rawValue);
      } else {
        requestAnimationFrame(look);
      }
    };
    look();
  } catch (error) {
    $("pair-status").textContent = "no camera: " + String(error.message || error);
  }
});

// A code scanned while this page is open changes only the hash; a
// reload takes the new ticket as a fresh visit does.
addEventListener("hashchange", () => {
  const fragment = location.hash.slice(1);
  if (fragment && !fragment.startsWith("/")) location.reload();
});

if ("serviceWorker" in navigator) {
  navigator.serviceWorker.register("sw.js").catch(() => {});
}

await init();
const saved = stored() || {};
key = saved.key || [];
const fresh = takeLink();
if (fresh) start(fresh);
else if (saved.link) start(saved.link);
else {
  let why = "";
  try {
    why = sessionStorage.getItem(STORE + ".why") || "";
    sessionStorage.removeItem(STORE + ".why");
  } catch (error) {
    why = "";
  }
  showPairing(why);
}
