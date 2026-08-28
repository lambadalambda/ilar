//! The HTTP surface: routes, the SSE envelope, and the token.
//!
//! **Reads are GET, writes are POST, and nothing else exists.** The
//! router is still where that boundary lives: every route names the
//! methods it answers, so anything else on a known path is a 405 from
//! the router itself rather than a check some future handler could
//! forget. The three POSTs are the whole write surface — a message, a
//! new session, an abort — and each of them does its work through
//! [`super::drive`], which runs turns the way `ilar exec` does. The
//! store is never written from a handler.
//!
//! Reads never go through [`ilar::session::SessionStore::load`]: P3
//! measured it failing 86.6% of calls against a live writer, because the
//! checkpoint stamp discipline belongs to the writer. Everything here
//! reads through [`SessionTail`], which is failure-free by construction.
//!
//! ```text
//! GET  /api/sessions                                  the head cache
//! GET  /api/sessions/{id}?from=&invocation=&limit=     one page, walking back
//! GET  /api/sessions/{id}/events?from=&token=          SSE
//! GET  /api/sessions/{id}/children
//! GET  /api/sessions/{id}/results/{tool_use_id}        full untruncated text
//! GET  /api/sessions/{id}/images/{event_id}/{n}        image bytes
//! GET  /  /app.css  /app.js                            the page
//! GET  /vendor/{preact,hooks,htm}.module.js            its ESM modules
//! POST /api/sessions            {prompt,cwd?,model?}   create and run
//! POST /api/sessions/{id}/message        {text}        steer or start
//! POST /api/sessions/{id}/abort                        cancel a driven turn
//! ```
//!
//! Two cursors, deliberately different, because they count different
//! things:
//!
//! - `cursor` indexes the **folded** canonical stream — what a
//!   transcript page walks back through (`?from=<cursor>` returns the
//!   page before it).
//! - `line` is the **physical** line of the log, monotonic forever
//!   (P5), which is what SSE `id:` carries and what `Last-Event-ID` or
//!   `?from=` resumes on.
//!
//! The SSE envelope:
//!
//! ```text
//! id: 42
//! event: append
//! data: {"line":42,"event":{…}}
//!
//! event: rewind    data: {"line":43,"to":7,"event":{…}}   (id: 43)
//! event: resync    data: {"line":43}   the view is stale; reload the transcript
//! event: deleted   data: {}            terminal
//! event: error     data: {"message":"…"}  terminal; the store's own words
//! event: error     data: {"message":"…","scope":"turn"}  a turn failed here
//! event: delta     data: {"type":"text_delta","text":"on "}   no id: ephemeral
//! ```
//!
//! A client folds `append`/`rewind` onto the transcript it fetched with
//! the same two lines the store uses: `rewind` truncates to `to`,
//! anything else pushes. `resync` means a line was missed (a lagging
//! subscriber, a repaired tail) and only a re-fetch is honest.
//!
//! `delta` is the running turn's scratch (see
//! [`ilar::session::LiveScratch`]) and is the one frame that carries no
//! `id:` — deliberately, because it is not a line of the log and must
//! never enter `Last-Event-ID` replay: a reconnect resumes on the last
//! *committed* line, and whatever was streaming arrives again as itself
//! or as the committed event it became. `{"type":"reset"}` retires the
//! client's streaming row; the committed `append` does the same.
//!
//! A `scope: "turn"` error is the one frame that does not come from the
//! store: a turn this server ran failed, and the log has no line that
//! says so (a provider failure is a `Diagnostic` block the projection
//! drops; a failure before the loop is never written at all). The tail
//! is unharmed and the stream stays open.
//!
//! Every request must name this server by an IP literal or `localhost`
//! before any of the above happens — [`require_known_host`], the
//! DNS-rebinding gate, which runs ahead of the token because a loopback
//! bind has no token to run.
//!
//! Auth: a loopback bind has none — anything that can reach it can read
//! the store directly. Any other bind requires a bearer token, and so
//! does a loopback bind with `ILAR_SERVE_TOKEN` set, because pinning a
//! token is an explicit request for one. Comparison is constant-time;
//! a failure is 401 with an empty body, on every path including
//! unmatched ones, so the token is not an oracle for what exists — the
//! sole exception being the static assets, which have to load before the
//! page can read the token out of the fragment ([`PUBLIC_PATHS`]).

use std::collections::VecDeque;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Router, middleware};
use futures::Stream;
use serde::Deserialize;
use serde_json::{Value, json};

use ilar::session::{SessionEvent, SessionStore, SessionTail, TailUpdate};
use tokio::sync::broadcast;

use super::drive::{Drive, DriveError, Fate, NewSession, TurnFailure};
use super::view::{
    invocation_slice, live_reset, project_event, project_events, project_live_delta, usage_totals,
};
use super::watch::{LiveMessage, SessionEntry, TailEnd, TailMessage, Watcher, next_message};

/// Events per transcript page. P7's worst session is 906 events; five
/// pages of it is a scroll, not a stall.
const PAGE_EVENTS: usize = 200;
/// The most a client may ask for in one page.
const MAX_PAGE_EVENTS: usize = 1_000;
/// A pinned token, and the name the docs use.
pub(crate) const TOKEN_ENV: &str = "ILAR_SERVE_TOKEN";
/// Frequency of the SSE comment frame that keeps proxies and idle
/// connections from dropping a quiet session.
const KEEP_ALIVE: Duration = Duration::from_secs(15);

/// Image types the browser is told to render. Anything else is served
/// as bytes: the media type comes out of a session file, and echoing an
/// arbitrary string into a response header is how header injection
/// starts.
const RENDERABLE_IMAGES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

#[derive(Clone)]
pub(crate) struct ServeState {
    pub(crate) watcher: Watcher,
    /// `None` on a loopback bind with no pinned token: no auth at all.
    pub(crate) token: Option<Arc<str>>,
    /// The write path: the turns this process is running.
    pub(crate) drive: Arc<Drive>,
    /// The address this server answers on, and therefore the only one a
    /// request may name — see [`require_known_host`].
    pub(crate) bind: SocketAddr,
}

pub(crate) fn router(state: ServeState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.css", get(stylesheet))
        .route("/app.js", get(script))
        .route("/vendor/preact.module.js", get(vendor_preact))
        .route("/vendor/hooks.module.js", get(vendor_hooks))
        .route("/vendor/htm.module.js", get(vendor_htm))
        .route("/api/sessions", get(sessions).post(create_session))
        .route("/api/sessions/{id}", get(transcript))
        .route("/api/sessions/{id}/events", get(events))
        .route("/api/sessions/{id}/children", get(children))
        .route("/api/sessions/{id}/results/{tool_use_id}", get(result_text))
        .route("/api/sessions/{id}/images/{event_id}/{n}", get(image))
        .route("/api/sessions/{id}/message", post(message))
        .route("/api/sessions/{id}/abort", post(abort))
        // On the whole router, fallback included: a 404 must not be
        // reachable without the token either.
        .layer(middleware::from_fn_with_state(state.clone(), require_token))
        // Outermost, so it runs *before* the token check: on a loopback
        // bind there is no token to fail, and this is the only thing
        // between a hostile page and a turn on this machine.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_known_host,
        ))
        .with_state(state)
}

// ------------------------------------------------------- host and auth

/// DNS rebinding, refused at the door.
///
/// A loopback bind is tokenless by design, and "can reach loopback"
/// meant "is already on this machine" — until a browser is the one
/// reaching. A page on `evil.com` whose DNS answers 127.0.0.1 on its
/// second lookup is, to the browser, *same-origin with this server*: it
/// can read every transcript and, now that there is a write path, POST a
/// prompt and have this machine run it. The attacker controls
/// everything about that request except one header: `Host` still says
/// `evil.com`, because that is the name the page was loaded from.
///
/// So the rule is a name rule. A request may name this server by an IP
/// literal or by `localhost` — which is exactly what the URL `ilar
/// serve` prints contains, and what an SSH tunnel gives — and a port,
/// when it names one, must be the bound port. Any other name is refused
/// with 403 and the reason, before anything else looks at the request.
/// An `Origin`, when the browser sends one, has to pass the same test:
/// a cross-origin `fetch` reaches the socket even when its response
/// would be unreadable, and a blind POST is enough to start a turn.
///
/// A missing `Host` is allowed only on a loopback bind. HTTP/1.1
/// requires the header, so what is left is HTTP/1.0 and hand-written
/// clients — local tools, in practice — and a browser is never among
/// them.
async fn require_known_host(
    State(state): State<ServeState>,
    request: Request,
    next: Next,
) -> Response {
    let host = text_header(request.headers(), &header::HOST);
    if !host_allowed(host.as_deref(), &state.bind) {
        return refuse_name("Host", host.as_deref(), &state.bind);
    }
    let origin = text_header(request.headers(), &header::ORIGIN);
    if let Some(origin) = &origin
        && !origin_allowed(origin, &state.bind)
    {
        return refuse_name("Origin", Some(origin), &state.bind);
    }
    next.run(request).await
}

fn text_header(headers: &HeaderMap, name: &header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn refuse_name(header: &str, value: Option<&str>, bind: &SocketAddr) -> Response {
    let named = value.unwrap_or("(absent)");
    ApiError::new(
        StatusCode::FORBIDDEN,
        format!(
            "{header} {named:?} is not this server's address: ilar serve answers to {bind} \
             (an IP literal or localhost, on port {}) and refuses any other name, because a \
             name is what a DNS-rebinding attack has to use. Open the URL ilar serve printed.",
            bind.port()
        ),
    )
    .into_response()
}

/// Whether a `Host` may name this server. An IP literal cannot be
/// rebound — the browser sends back whatever the address bar holds — so
/// any literal is accepted, and so is `localhost`; a hostname is not.
fn host_allowed(host: Option<&str>, bind: &SocketAddr) -> bool {
    let Some(host) = host else {
        return bind.ip().is_loopback();
    };
    let Some((name, port)) = split_host_port(host) else {
        return false;
    };
    // A `Host` without a port is port 80, i.e. something in front; the
    // name is what matters either way. A port that disagrees with the
    // bind is not this server being addressed.
    if port.is_some_and(|port| port != bind.port()) {
        return false;
    }
    name.eq_ignore_ascii_case("localhost") || name.parse::<std::net::IpAddr>().is_ok()
}

/// The same test on an `Origin`, which is a scheme and an authority.
fn origin_allowed(origin: &str, bind: &SocketAddr) -> bool {
    origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
        .is_some_and(|authority| host_allowed(Some(authority), bind))
}

/// `host[:port]`, with the bracket form IPv6 needs. `None` for anything
/// that is not one of those two shapes.
fn split_host_port(host: &str) -> Option<(&str, Option<u16>)> {
    let host = host.trim();
    if host.is_empty() {
        return None;
    }
    if let Some(rest) = host.strip_prefix('[') {
        let (name, rest) = rest.split_once(']')?;
        let port = match rest {
            "" => None,
            rest => Some(rest.strip_prefix(':')?.parse().ok()?),
        };
        return Some((name, port));
    }
    match host.rsplit_once(':') {
        Some((name, port)) => Some((name, Some(port.parse().ok()?))),
        None => Some((host, None)),
    }
}

/// The token this bind requires, if any. Explicit pinning wins over the
/// loopback exemption: someone who sets the variable wants the check.
pub(crate) fn required_token(bind: &SocketAddr, configured: Option<String>) -> Option<String> {
    match configured.map(|token| token.trim().to_string()) {
        Some(token) if !token.is_empty() => Some(token),
        _ if bind.ip().is_loopback() => None,
        _ => Some(generate_token()),
    }
}

/// 256 bits from the OS, hex-encoded — long enough that the constant-
/// time compare below is the only thing standing between an attacker
/// and nothing at all.
pub(crate) fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the OS random source");
    bytes
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// The URL to print. The token rides in the fragment, which browsers do
/// not send upstream and servers do not log; the page moves it into
/// sessionStorage and appends `?token=` where `EventSource` needs it.
pub(crate) fn url_for(address: &SocketAddr, token: Option<&str>) -> String {
    match token {
        // Percent-encoded, because a pinned token is whatever the user
        // put in `ILAR_SERVE_TOKEN`: a space or a `#` printed raw is a
        // URL that does not survive a copy into a browser.
        Some(token) => format!("http://{address}/#token={}", percent_encode(token)),
        None => format!("http://{address}/"),
    }
}

/// The static files, and the one exception to the gate. The token rides
/// in the URL fragment, which a browser never sends upstream — so gating
/// the page that reads that fragment would make the token impossible to
/// deliver, and `--open` would open a 401. These bytes are identical for
/// every install, already public inside the binary, and say nothing
/// whatever about the store; the data behind them stays gated, including
/// the fallback. The vendored modules are here for the same reason and
/// with more force: a module the page imports is fetched by the module
/// loader, which cannot be handed a token either.
const PUBLIC_PATHS: [&str; 6] = [
    "/",
    "/app.css",
    "/app.js",
    "/vendor/preact.module.js",
    "/vendor/hooks.module.js",
    "/vendor/htm.module.js",
];

async fn require_token(State(state): State<ServeState>, request: Request, next: Next) -> Response {
    let Some(expected) = state.token.as_deref() else {
        return next.run(request).await;
    };
    if PUBLIC_PATHS.contains(&request.uri().path()) {
        return next.run(request).await;
    }
    let presented = bearer(request.headers()).or_else(|| query_token(request.uri().query()));
    match presented {
        Some(presented) if constant_time_eq(&presented, expected) => next.run(request).await,
        // No body: nothing to tell an unauthenticated caller, not even
        // which of the two ways of presenting a token was wrong.
        _ => StatusCode::UNAUTHORIZED.into_response(),
    }
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim().to_string())
}

/// `?token=` for `EventSource` and `<img>`, neither of which can set a
/// header. The middleware reads the raw query rather than a parsed one,
/// so the percent-decode is this function's job: a generated token is
/// hex and needs none, but a pinned `ILAR_SERVE_TOKEN` can hold anything
/// at all, and the page sends it through `encodeURIComponent`.
///
/// `+` is left alone. It is a space only in form encoding, which nothing
/// here produces, and treating it as one would break every token with a
/// literal `+` in it.
fn query_token(query: Option<&str>) -> Option<String> {
    query?
        .split('&')
        .find_map(|pair| pair.strip_prefix("token="))
        .map(percent_decode)
}

/// `%XX` back to bytes, then to text. Invalid escapes are kept verbatim:
/// this feeds a comparison, and a token that does not decode is simply a
/// token that will not match.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match hex_pair(bytes.get(index + 1..index + 3)) {
            Some(byte) if bytes[index] == b'%' => {
                decoded.push(byte);
                index += 3;
            }
            _ => {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn hex_pair(pair: Option<&[u8]>) -> Option<u8> {
    let pair = pair?;
    let digit = |byte: u8| (byte as char).to_digit(16).map(|digit| digit as u8);
    Some(digit(*pair.first()?)? << 4 | digit(*pair.get(1)?)?)
}

/// The unreserved set survives; everything else is escaped. Stricter
/// than `encodeURIComponent`, which is the safe direction — the page
/// decodes with `decodeURIComponent`, and that accepts both.
fn percent_encode(value: &str) -> String {
    value.bytes().fold(String::new(), |mut encoded, byte| {
        use std::fmt::Write as _;
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(byte as char);
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
        encoded
    })
}

/// Compare without an early exit. The length is not secret — the token
/// is a fixed 64 hex characters — but the bytes are.
fn constant_time_eq(left: &str, right: &str) -> bool {
    let (left, right) = (left.as_bytes(), right.as_bytes());
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

// -------------------------------------------------------------- errors

/// A failed request, as JSON. Store errors keep their own words: they
/// name the session and the line, which is what a bug report needs.
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }
}

impl From<std::io::Error> for ApiError {
    fn from(error: std::io::Error) -> Self {
        let status = match error.kind() {
            std::io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
            std::io::ErrorKind::InvalidInput => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self::new(status, error.to_string())
    }
}

impl From<DriveError> for ApiError {
    fn from(error: DriveError) -> Self {
        Self::new(error.status(), error.message())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

/// Session reads are synchronous file IO (a 1.65 MB worst case, P7), so
/// they go to a blocking thread rather than stalling a runtime worker.
async fn blocking<T, F>(task: F) -> Result<T, ApiError>
where
    F: FnOnce() -> std::io::Result<T> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(task).await {
        Ok(result) => result.map_err(ApiError::from),
        Err(error) => Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("reader task failed: {error}"),
        )),
    }
}

/// One session's folded canonical view and the physical line it stands
/// at. The tail reader is the only one that never loses a race with a
/// running turn.
fn read_session(store: &SessionStore, id: &str) -> std::io::Result<(Vec<SessionEvent>, usize)> {
    let mut tail = SessionTail::open(store, id)?;
    tail.poll()?;
    Ok((tail.events().to_vec(), tail.line()))
}

async fn session_events(
    state: &ServeState,
    id: &str,
) -> Result<(Vec<SessionEvent>, usize), ApiError> {
    let store = state.watcher.store().clone();
    let id = id.to_string();
    blocking(move || read_session(&store, &id)).await
}

// ------------------------------------------------------------ handlers

/// The listing, straight off the head cache: cheap enough to poll, and
/// grouped by `cwd` on the client (the server has no privileged
/// directory of its own).
async fn sessions(State(state): State<ServeState>) -> Json<Value> {
    Json(json!({
        "sessions": state
            .watcher
            .sessions()
            .iter()
            .map(|entry| summary(entry, &state.drive))
            .collect::<Vec<_>>(),
    }))
}

async fn children(
    State(state): State<ServeState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!({
        "children": state
            .watcher
            .children(&id)
            .iter()
            .map(|entry| summary(entry, &state.drive))
            .collect::<Vec<_>>(),
    })))
}

// --------------------------------------------------------------- write

/// A new session and its first turn. The id comes back immediately and
/// the turn runs behind it: the page follows on the stream it would have
/// opened anyway, so there is nothing to wait for here and no long
/// request to lose.
async fn create_session(
    State(state): State<ServeState>,
    Json(body): Json<CreateBody>,
) -> Result<Json<Value>, ApiError> {
    let id = state
        .drive
        .create(NewSession {
            prompt: body.prompt,
            cwd: body.cwd,
            model: body.model,
        })
        .await?;
    Ok(Json(json!({ "id": id, "fate": Fate::Started.as_str() })))
}

#[derive(Debug, Deserialize)]
struct CreateBody {
    prompt: String,
    cwd: Option<String>,
    model: Option<String>,
}

/// A message for a session: a steer when this process is running a turn
/// there, a new turn when it is not — and a 409 when another process
/// holds the writer, which is the page's cue to say "watching only"
/// rather than to retry.
async fn message(
    State(state): State<ServeState>,
    Path(id): Path<String>,
    Json(body): Json<MessageBody>,
) -> Result<Json<Value>, ApiError> {
    let fate = state.drive.message(&id, &body.text).await?;
    Ok(Json(json!({ "fate": fate.as_str() })))
}

#[derive(Debug, Deserialize)]
struct MessageBody {
    text: String,
}

/// Stop the turn this process is running. A session driven by some other
/// ilar is not this server's to stop, and says so.
async fn abort(
    State(state): State<ServeState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let fate = state.drive.abort(&id)?;
    Ok(Json(json!({ "fate": fate.as_str() })))
}

#[derive(Debug, Deserialize)]
struct TranscriptQuery {
    /// Exclusive upper bound in the folded stream; the page before it.
    from: Option<usize>,
    /// Narrow to one subagent invocation, by the parent's tool call id.
    invocation: Option<String>,
    limit: Option<usize>,
}

async fn transcript(
    State(state): State<ServeState>,
    Path(id): Path<String>,
    Query(query): Query<TranscriptQuery>,
) -> Result<Json<Value>, ApiError> {
    let (events, line) = session_events(&state, &id).await?;
    let view = match &query.invocation {
        Some(invocation) => invocation_slice(&events, invocation),
        None => &events[..],
    };
    let (start, end) = page_bounds(view.len(), query.from, query.limit);
    Ok(Json(json!({
        "id": id,
        "session": state
            .watcher
            .head(&id)
            .as_ref()
            .map_or(Value::Null, |entry| summary(entry, &state.drive)),
        "events": project_events(&view[start..end]),
        "cursor": start,
        "has_more": start > 0,
        "count": view.len(),
        "line": line,
        "usage": usage_totals(view),
    })))
}

/// A transcript is read backwards: without `from` the newest page is
/// returned, and `cursor` is what the next request passes back.
fn page_bounds(count: usize, from: Option<usize>, limit: Option<usize>) -> (usize, usize) {
    let limit = limit.unwrap_or(PAGE_EVENTS).clamp(1, MAX_PAGE_EVENTS);
    let end = from.unwrap_or(count).min(count);
    (end.saturating_sub(limit), end)
}

/// The untruncated text behind a `truncated: true` tool result. Plain
/// text, because that is what it is — the projection's bounded copy is
/// the only one that gets shaped for a row.
async fn result_text(
    State(state): State<ServeState>,
    Path((id, tool_use_id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let (events, _) = session_events(&state, &id).await?;
    let text = events
        .iter()
        .find_map(|event| match event {
            SessionEvent::ToolResult {
                tool_use_id: current,
                content,
                ..
            } if *current == tool_use_id => Some(content.clone()),
            _ => None,
        })
        .ok_or_else(|| ApiError::not_found(format!("no tool result for {tool_use_id}")))?;
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response())
}

/// The bytes the projection deliberately left behind: one image, by the
/// event that carries it and its index in that event.
async fn image(
    State(state): State<ServeState>,
    Path((id, event_id, index)): Path<(String, String, usize)>,
) -> Result<Response, ApiError> {
    use base64::Engine as _;

    let (events, _) = session_events(&state, &id).await?;
    let image = events
        .iter()
        .find_map(|event| match event {
            SessionEvent::UserMessage { id, images, .. }
            | SessionEvent::ToolResult { id, images, .. }
                if *id == event_id =>
            {
                images.get(index)
            }
            _ => None,
        })
        .ok_or_else(|| ApiError::not_found(format!("no image {index} on event {event_id}")))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&image.data)
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("image {index} on event {event_id} is not valid base64: {error}"),
            )
        })?;
    Ok((
        [(header::CONTENT_TYPE, content_type(&image.media_type))],
        bytes,
    )
        .into_response())
}

fn content_type(media_type: &str) -> &'static str {
    RENDERABLE_IMAGES
        .into_iter()
        .find(|renderable| *renderable == media_type)
        .unwrap_or("application/octet-stream")
}

#[derive(Debug, Deserialize)]
struct EventsQuery {
    /// The physical line already seen; the stream resumes after it.
    from: Option<usize>,
    /// `EventSource` cannot set headers, so the token may ride here.
    #[allow(dead_code)]
    token: Option<String>,
}

async fn events(
    State(state): State<ServeState>,
    Path(id): Path<String>,
    Query(query): Query<EventsQuery>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let watcher = state.watcher.clone();
    let subscribed = id.clone();
    let subscription = blocking(move || watcher.subscribe(&subscribed)).await?;

    // The header wins. `EventSource` reconnects reuse the *original*
    // URL, so `?from=` is frozen at the moment the tab attached while
    // `Last-Event-ID` holds the client's true position: preferring the
    // query would replay everything since the tab opened, on every
    // reconnect, for the client to fold in twice. A first connection has
    // no header and names its starting line in the query instead,
    // usually the transcript's `line`.
    let resume = last_event_id(&headers).or(query.from);
    let mut pending: VecDeque<Event> = VecDeque::new();
    let mut last_line = subscription.line;
    let ended = subscription.ended.is_some();
    match subscription.ended {
        Some(TailEnd::Deleted) => pending.push_back(named("deleted", &json!({}))),
        Some(TailEnd::Failed(message)) => {
            pending.push_back(named("error", &json!({ "message": message })));
        }
        None => {
            if let Some(from) = resume {
                if from < subscription.line {
                    let (caught_up, line) = catch_up(&state, &id, from, subscription.line).await;
                    pending = caught_up;
                    last_line = line;
                } else {
                    last_line = from;
                }
            }
            // After every committed line, because that is where the
            // running step stands: a connection opened mid-step gets the
            // row as it already is, then continues it. Unconditional on
            // resume — deltas carry no id, so a reconnecting client threw
            // its copy away and needs this one.
            pending.extend(
                subscription
                    .live
                    .iter()
                    .map(|delta| named("delta", &project_live_delta(delta))),
            );
        }
    }

    let feed = Feed {
        receiver: subscription.receiver,
        failures: Some(state.drive.failures()),
        id,
        pending,
        last_line,
        done: ended,
    };
    Ok(Sse::new(futures::stream::unfold(feed, Feed::next))
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE)))
}

/// The lines between a client's last-seen one and the tailer's snapshot,
/// re-read from the file. Everything the broadcast then repeats is
/// dropped by line number, so a resume never duplicates.
async fn catch_up(
    state: &ServeState,
    id: &str,
    from: usize,
    snapshot_line: usize,
) -> (VecDeque<Event>, usize) {
    let store = state.watcher.store().clone();
    let owned = id.to_string();
    let replay = blocking(move || {
        let mut tail = SessionTail::open_at(&store, &owned, from)?;
        let updates = tail.poll()?;
        Ok((updates, tail.line()))
    })
    .await;
    match replay {
        Ok((updates, line)) => {
            let mut frames = VecDeque::with_capacity(updates.len());
            let mut last = from;
            for update in updates {
                let frame = frame(TailMessage::Update(update), last);
                last = frame.line;
                frames.push_back(frame.event);
            }
            (frames, line.max(last))
        }
        // The requested line is gone (a repaired or replaced file): the
        // client cannot be resumed, only told to reload.
        Err(_) => (
            VecDeque::from([named("resync", &json!({ "line": from }))]),
            snapshot_line,
        ),
    }
}

fn last_event_id(headers: &HeaderMap) -> Option<usize> {
    headers
        .get("last-event-id")?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// The SSE stream's state: what is queued, and where the client is.
struct Feed {
    receiver: tokio::sync::broadcast::Receiver<TailMessage>,
    /// Turn failures from this process's own write path, or `None` once
    /// that channel is gone. Merged in here because the log cannot carry
    /// them: see [`TurnFailure`].
    failures: Option<tokio::sync::broadcast::Receiver<TurnFailure>>,
    /// The session this stream follows — the failures channel is the
    /// whole process's, so its frames are filtered on this.
    id: String,
    pending: VecDeque<Event>,
    last_line: usize,
    done: bool,
}

/// Where the next frame came from.
// Same trade `TailMessage` itself makes: boxing the line-bearing variant
// to shrink the three rare ones would cost an allocation per line.
#[allow(clippy::large_enum_variant)]
enum Source {
    Tail(TailMessage),
    /// A turn this server was running on this session failed.
    Failed(String),
    /// A failure for some other session, or one this stream fell behind.
    Ignored,
    /// The write path is gone; stop watching it.
    NoMoreFailures,
}

impl Feed {
    async fn next(mut self) -> Option<(Result<Event, Infallible>, Self)> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Some((Ok(event), self));
            }
            if self.done {
                return None;
            }
            let message = match self.source().await? {
                // A failed turn is not a failed *tail*: the log is
                // intact and the stream stays open. `scope` is what says
                // so, for a client that wants to tell this from the
                // store's own terminal error.
                Source::Failed(message) => {
                    let event = named("error", &json!({ "message": message, "scope": "turn" }));
                    return Some((Ok(event), self));
                }
                Source::Ignored => continue,
                Source::NoMoreFailures => {
                    self.failures = None;
                    continue;
                }
                Source::Tail(message) => message,
            };
            // Lines the catch-up read already sent: the broadcast and
            // the file overlap by design, and the line number is what
            // makes the overlap harmless.
            if let Some(line) = line_of(&message)
                && line <= self.last_line
            {
                continue;
            }
            let frame = frame(message, self.last_line);
            self.last_line = frame.line;
            self.done = frame.terminal;
            return Some((Ok(frame.event), self));
        }
    }

    /// The next thing to happen on either channel. `None` ends the
    /// stream: the tailer is gone for good.
    async fn source(&mut self) -> Option<Source> {
        let Feed {
            receiver,
            failures,
            id,
            ..
        } = self;
        match failures {
            Some(channel) => tokio::select! {
                message = next_message(receiver) => message.map(Source::Tail),
                failure = channel.recv() => Some(match failure {
                    Ok(failure) if failure.session_id == *id => Source::Failed(failure.message),
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => Source::Ignored,
                    Err(broadcast::error::RecvError::Closed) => Source::NoMoreFailures,
                }),
            },
            None => next_message(receiver).await.map(Source::Tail),
        }
    }
}

fn line_of(message: &TailMessage) -> Option<usize> {
    match message {
        TailMessage::Update(
            TailUpdate::Appended { line, .. } | TailUpdate::Rewound { line, .. },
        ) => Some(*line),
        _ => None,
    }
}

struct Frame {
    event: Event,
    /// The line the client has now seen; only the two line-bearing
    /// kinds move it.
    line: usize,
    terminal: bool,
}

fn frame(message: TailMessage, last_line: usize) -> Frame {
    match message {
        TailMessage::Update(TailUpdate::Appended { line, event }) => Frame {
            event: named(
                "append",
                &json!({ "line": line, "event": project_event(&event) }),
            )
            .id(line.to_string()),
            line,
            terminal: false,
        },
        TailMessage::Update(TailUpdate::Rewound { line, to, event }) => Frame {
            event: named(
                "rewind",
                &json!({ "line": line, "to": to, "event": project_event(&event) }),
            )
            .id(line.to_string()),
            line,
            terminal: false,
        },
        // The view is stale and the client must re-fetch it; until it
        // does, nothing about its old position can be trusted — so the
        // duplicate filter is disarmed rather than left pointing at a
        // line the file may no longer have.
        TailMessage::Update(TailUpdate::Resync) => Frame {
            event: named("resync", &json!({ "line": last_line })),
            line: 0,
            terminal: false,
        },
        TailMessage::Update(TailUpdate::Deleted) => Frame {
            event: named("deleted", &json!({})),
            line: last_line,
            terminal: true,
        },
        TailMessage::Failed(message) => Frame {
            event: named("error", &json!({ "message": message })),
            line: last_line,
            terminal: true,
        },
        // No `id:`, on purpose: a scratch line is not a line of the log,
        // and a reconnect must resume on the last committed one.
        TailMessage::Live(live) => Frame {
            event: named(
                "delta",
                &match live {
                    LiveMessage::Reset => live_reset(),
                    LiveMessage::Delta(delta) => project_live_delta(&delta),
                },
            ),
            line: last_line,
            terminal: false,
        },
    }
}

fn named(event: &str, data: &Value) -> Event {
    Event::default().event(event).data(data.to_string())
}

/// One listing row. `state` is `working`, `stalled` or `idle`, read off
/// the session's live-turn scratch rather than guessed from its log's
/// mtime — and never a lock probe, because taking the writer lease to
/// answer a *listing* would evict whoever is writing, which is the one
/// thing a poll must never do (the write routes take it deliberately,
/// once, for a turn). `activity` names the tool a working session is
/// running, when it
/// is running one. `context_limit` is the window the page's context bar
/// measures against, `null` for a model this binary has no catalog row
/// for — a listing row carries it because the panel needs it before any
/// page of the transcript has reached the `meta` line. `driven` is the
/// one thing the store cannot say: whether *this* server is running the
/// turn, which is the difference between an abort button and a dot. It
/// is a new field beside `state`, not a fourth value of it — a session
/// can be working under a TUI, and the page must not offer to stop it.
fn summary(entry: &SessionEntry, drive: &Drive) -> Value {
    json!({
        "driven": drive.drives(&entry.head.id),
        "id": entry.head.id,
        "title": entry.head.title,
        "cwd": entry.head.meta.cwd.as_ref().map(|cwd| cwd.display().to_string()),
        "agent": entry.head.meta.agent,
        "model": entry.head.meta.model,
        "context_limit": super::view::context_limit(&entry.head.meta.model),
        "parent_id": entry.head.meta.parent_id,
        "modified": rfc3339(entry.head.modified),
        "state": entry.state.as_str(),
        "activity": entry.activity,
    })
}

fn rfc3339(time: std::time::SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(time).to_rfc3339()
}

// -------------------------------------------------------------- assets

/// The page is a handful of files compiled into the binary: one artifact
/// to ship, and no path a request could traverse. Three of them are the
/// page itself and three are the vendored ESM modules it imports —
/// preact, its hooks and htm, pinned and copied verbatim, so `ilar serve`
/// still works on a plane and still has no build step.
async fn index() -> Response {
    asset(
        "text/html; charset=utf-8",
        include_str!("assets/index.html"),
    )
}

async fn stylesheet() -> Response {
    asset("text/css; charset=utf-8", include_str!("assets/app.css"))
}

async fn script() -> Response {
    script_asset(include_str!("assets/app.js"))
}

async fn vendor_preact() -> Response {
    script_asset(include_str!("assets/vendor/preact.module.js"))
}

async fn vendor_hooks() -> Response {
    script_asset(include_str!("assets/vendor/hooks.module.js"))
}

async fn vendor_htm() -> Response {
    script_asset(include_str!("assets/vendor/htm.module.js"))
}

fn script_asset(body: &'static str) -> Response {
    asset("text/javascript; charset=utf-8", body)
}

fn asset(content_type: &'static str, body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, content_type)], body).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(text: &str) -> SocketAddr {
        text.parse().unwrap()
    }

    #[test]
    fn a_loopback_bind_needs_no_token_and_anything_else_does() {
        assert_eq!(required_token(&addr("127.0.0.1:7777"), None), None);
        assert_eq!(required_token(&addr("[::1]:7777"), None), None);

        let generated = required_token(&addr("192.168.1.10:7777"), None).unwrap();
        assert_eq!(generated.len(), 64, "256 bits, hex");
        assert!(generated.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(
            generated,
            generate_token(),
            "one per process, not a constant"
        );

        // A pinned token is an explicit request for the check, even on
        // loopback; blank means unset.
        assert_eq!(
            required_token(&addr("127.0.0.1:7777"), Some("  hunter2 ".into())),
            Some("hunter2".into())
        );
        assert_eq!(
            required_token(&addr("127.0.0.1:7777"), Some("  ".into())),
            None
        );
        assert_eq!(
            required_token(&addr("0.0.0.0:7777"), Some("pinned".into())),
            Some("pinned".into())
        );
    }

    #[test]
    fn the_printed_url_carries_a_token_in_the_fragment_only() {
        assert_eq!(
            url_for(&addr("127.0.0.1:7777"), None),
            "http://127.0.0.1:7777/"
        );
        assert_eq!(
            url_for(&addr("10.0.0.2:7777"), Some("abc")),
            "http://10.0.0.2:7777/#token=abc",
            "a fragment is not sent upstream and not logged"
        );
        // A pinned token is whatever the environment held, and a URL is
        // printed to be copied: the escaping is the server's job.
        assert_eq!(
            url_for(&addr("10.0.0.2:7777"), Some("p@ss word%1")),
            "http://10.0.0.2:7777/#token=p%40ss%20word%251"
        );
    }

    /// The rebinding gate. A hostile page's request differs from the
    /// page's own in exactly one place it cannot forge — the name it was
    /// loaded from — so that is what is checked.
    #[test]
    fn only_this_servers_own_address_may_be_named() {
        let bind = addr("127.0.0.1:4527");
        assert!(host_allowed(Some("127.0.0.1:4527"), &bind));
        assert!(host_allowed(Some("localhost:4527"), &bind));
        assert!(host_allowed(Some("LocalHost:4527"), &bind));
        assert!(host_allowed(Some("[::1]:4527"), &bind));
        // A bare host is something in front of us; the name still has to
        // be one a rebinding attack cannot use.
        assert!(host_allowed(Some("127.0.0.1"), &bind));
        assert!(host_allowed(Some("localhost"), &bind));

        assert!(!host_allowed(Some("evil.com"), &bind), "the whole point");
        assert!(!host_allowed(Some("evil.com:4527"), &bind));
        assert!(!host_allowed(Some("localhost.evil.com:4527"), &bind));
        assert!(!host_allowed(Some("127.0.0.1:9999"), &bind), "another port");
        assert!(!host_allowed(Some(""), &bind));

        // No `Host` at all: HTTP/1.0 and hand-written clients, which are
        // local by nature. A browser always sends one.
        assert!(host_allowed(None, &bind));
        assert!(!host_allowed(None, &addr("10.0.0.2:7777")));

        // A non-loopback bind is named by its own address, and by the
        // tunnel that reaches it.
        let public = addr("10.0.0.2:7777");
        assert!(host_allowed(Some("10.0.0.2:7777"), &public));
        assert!(host_allowed(Some("localhost:7777"), &public));
        assert!(!host_allowed(Some("box.local:7777"), &public));

        assert!(origin_allowed("http://127.0.0.1:4527", &bind));
        assert!(origin_allowed("https://localhost:4527", &bind));
        assert!(!origin_allowed("http://evil.com", &bind));
        assert!(!origin_allowed("null", &bind), "a sandboxed frame");
        assert!(!origin_allowed("127.0.0.1:4527", &bind), "not an origin");
    }

    #[test]
    fn a_host_splits_into_a_name_and_an_optional_port() {
        assert_eq!(
            split_host_port("localhost:80"),
            Some(("localhost", Some(80)))
        );
        assert_eq!(split_host_port("localhost"), Some(("localhost", None)));
        assert_eq!(split_host_port("[::1]:4527"), Some(("::1", Some(4527))));
        assert_eq!(split_host_port("[::1]"), Some(("::1", None)));
        assert_eq!(split_host_port("localhost:nope"), None);
        assert_eq!(split_host_port(""), None);
    }

    #[test]
    fn a_token_is_read_from_the_header_or_the_query() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer abc123".parse().unwrap());
        assert_eq!(bearer(&headers), Some("abc123".into()));

        headers.insert(header::AUTHORIZATION, "bearer abc123".parse().unwrap());
        assert_eq!(
            bearer(&headers),
            Some("abc123".into()),
            "scheme is case-insensitive"
        );

        headers.insert(header::AUTHORIZATION, "Basic abc123".parse().unwrap());
        assert_eq!(bearer(&headers), None);

        assert_eq!(query_token(Some("token=abc123")), Some("abc123".into()));
        assert_eq!(query_token(Some("from=3&token=abc")), Some("abc".into()));
        assert_eq!(query_token(Some("from=3")), None);
        assert_eq!(query_token(None), None);

        // A pinned token is arbitrary text and the page sends it through
        // `encodeURIComponent`; comparing the raw query against it would
        // 401 the SSE and image routes forever.
        assert_eq!(
            query_token(Some("token=p%40ss%20word%25")),
            Some("p@ss word%".into())
        );
        assert_eq!(
            query_token(Some("from=3&token=a%2Bb")),
            Some("a+b".into()),
            "an encoded plus is a plus"
        );
        assert_eq!(
            query_token(Some("token=a+b")),
            Some("a+b".into()),
            "and a bare one is not a space: nothing here is form-encoded"
        );
        // Nonsense decodes to itself and simply fails the comparison.
        assert_eq!(query_token(Some("token=%zz%")), Some("%zz%".into()));
    }

    /// The round trip the page performs: printed encoded, read back
    /// decoded, byte for byte.
    #[test]
    fn a_pinned_token_survives_the_url_it_is_printed_in() {
        for token in ["p@ss word", "100%", "a+b", "üni/code?&#", "plain"] {
            let url = url_for(&addr("127.0.0.1:4527"), Some(token));
            let printed = url.split_once("#token=").expect("a fragment").1;
            assert_eq!(
                query_token(Some(&format!("token={printed}"))).as_deref(),
                Some(token)
            );
        }
    }

    #[test]
    fn tokens_compare_without_an_early_exit() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "abcd"));
        assert!(!constant_time_eq("", "a"));
        assert!(constant_time_eq("", ""));
    }

    /// Paging walks backwards: the newest page first, then the one
    /// before it, until the cursor reaches the start.
    #[test]
    fn a_transcript_pages_backwards_to_the_beginning() {
        assert_eq!(page_bounds(500, None, Some(200)), (300, 500));
        assert_eq!(page_bounds(500, Some(300), Some(200)), (100, 300));
        assert_eq!(page_bounds(500, Some(100), Some(200)), (0, 100));
        assert_eq!(page_bounds(0, None, None), (0, 0));
        // Bounds are clamped, never trusted.
        assert_eq!(page_bounds(10, Some(99), None), (0, 10));
        assert_eq!(page_bounds(10, None, Some(0)), (9, 10));
        assert_eq!(
            page_bounds(10_000, None, Some(usize::MAX)),
            (10_000 - MAX_PAGE_EVENTS, 10_000)
        );
        assert_eq!(page_bounds(500, None, None), (500 - PAGE_EVENTS, 500));
    }

    #[test]
    fn only_known_image_types_reach_a_content_type_header() {
        assert_eq!(content_type("image/png"), "image/png");
        assert_eq!(content_type("image/webp"), "image/webp");
        assert_eq!(
            content_type("text/html\r\nX-Evil: 1"),
            "application/octet-stream",
            "a session file cannot dictate a response header"
        );
    }

    /// The two line-bearing frames carry the SSE id; the terminal and
    /// stale-view frames do not move the client's position.
    #[test]
    fn the_sse_envelope_ids_only_the_line_bearing_frames() {
        let event = SessionEvent::Topic {
            id: "topic-1".into(),
            text: "serve".into(),
            ts: chrono::Utc::now(),
        };
        let appended = frame(
            TailMessage::Update(TailUpdate::Appended {
                line: 42,
                event: event.clone(),
            }),
            7,
        );
        assert_eq!(appended.line, 42);
        assert!(!appended.terminal);

        let rewound = frame(
            TailMessage::Update(TailUpdate::Rewound {
                line: 43,
                to: 7,
                event,
            }),
            42,
        );
        assert_eq!(rewound.line, 43);

        assert_eq!(frame(TailMessage::Update(TailUpdate::Resync), 43).line, 0);
        let deleted = frame(TailMessage::Update(TailUpdate::Deleted), 43);
        assert_eq!(deleted.line, 43);
        assert!(deleted.terminal);
        assert!(frame(TailMessage::Failed("newer ilar?".into()), 43).terminal);
    }
}
