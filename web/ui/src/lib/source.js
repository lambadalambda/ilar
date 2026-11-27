// Where a session comes from. Three sources behind one set of calls:
//
// - serve: HTTP under /api with a bearer token, and an EventSource.
// - share: the whole payload is in the page (web::share writes
//   window.__ILAR_SHARE__); reads answer from it, writes are refused,
//   no stream opens.
// - remote: a paired phone; web/remote/remote.js sets
//   window.__ILAR_REMOTE__ before it loads this app, and reads, writes
//   and the stream go over the iroh wire.

const TOKEN_KEY = "ilar.serve.token";

export const SHARE = window.__ILAR_SHARE__ || null;
export const REMOTE = window.__ILAR_REMOTE__ || null;
// No server routes behind the page: thumbnails, child transcripts and
// new sessions have nowhere to come from.
export const DETACHED = Boolean(SHARE || REMOTE);

// The server prints the token in the URL fragment, which browsers do not
// send upstream: keep it in sessionStorage and strip it from the address.
function bootToken() {
  if (DETACHED) return "";
  try {
    const parts = location.hash.replace(/^#/, "").split("&");
    const carried = parts.find((part) => part.startsWith("token="));
    if (carried) {
      sessionStorage.setItem(TOKEN_KEY, decodeURIComponent(carried.slice(6)));
      const rest = parts.filter((part) => part && !part.startsWith("token="));
      history.replaceState(null, "", location.pathname + location.search + (rest.length ? "#" + rest.join("&") : "#/"));
    }
    return sessionStorage.getItem(TOKEN_KEY) || "";
  } catch (error) {
    return "";
  }
}

const token = bootToken();

export const apiPath = (id, suffix) => "/api/sessions/" + encodeURIComponent(id) + (suffix || "");

// EventSource and <img> cannot set a header, so those carry ?token=.
function withToken(path) {
  if (!token) return path;
  return path + (path.includes("?") ? "&" : "?") + "token=" + encodeURIComponent(token);
}

async function fetchPath(path) {
  const response = await fetch(path, { headers: token ? { Authorization: "Bearer " + token } : {} });
  if (!response.ok) throw new Error("GET " + path + " → " + response.status);
  return response;
}

// What a shared page would have fetched, by path.
let shareRoutes = null;
function fromShare(path) {
  if (!shareRoutes) {
    shareRoutes = new Map([
      ["/api/sessions", { sessions: [SHARE.session] }],
      [apiPath(SHARE.id), SHARE.page],
      [apiPath(SHARE.id, "/children"), { children: SHARE.children || [] }],
      ...Object.entries(SHARE.extra || {}),
    ]);
  }
  const body = shareRoutes.get(path);
  // A path it does not carry is a real absence, not an empty success.
  return body === undefined
    ? Promise.reject(new Error("this shared transcript does not carry " + path))
    : Promise.resolve(body);
}

export function api(path) {
  if (REMOTE) return REMOTE.api(path);
  if (SHARE) return fromShare(path);
  return fetchPath(path).then((response) => response.json());
}

export function apiText(path) {
  const text = (body) => (typeof body === "string" ? body : JSON.stringify(body));
  if (REMOTE) return REMOTE.api(path).then(text);
  if (SHARE) return fromShare(path).then(text);
  return fetchPath(path).then((response) => response.text());
}

/** The write path. A failure the server explains comes back as `{error}`
 * with a status; both ride on the thrown error (409 is a state). */
export async function post(path, body) {
  if (SHARE) throw new Error("this is a shared transcript — it cannot be written to");
  if (REMOTE) return REMOTE.post(path, body);
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

/** The live stream: an EventSource, or the wire's frames behind the same
 * three methods. None for a share. */
export function openStream(id, line) {
  if (SHARE) return null;
  if (REMOTE) return REMOTE.stream();
  return new EventSource(withToken(apiPath(id, "/events?from=" + line)));
}

/** An image's URL, or a promise of one; null where none can come. */
export function imageUrl(sessionId, eventId, n) {
  if (REMOTE) return REMOTE.image ? REMOTE.image(sessionId, eventId, n) : null;
  if (SHARE) return null;
  return withToken(apiPath(sessionId, "/images/" + encodeURIComponent(eventId) + "/" + n));
}

export const source = { api, openStream, SHARE: Boolean(SHARE) };
