//! The read-only HTTP surface: routes, the SSE envelope, and the token.
//!
//! **GET only.** Phase 2 reads the store and nothing else, and that
//! boundary is structural here — every route is registered with
//! [`axum::routing::get`], so anything else on a known path is a 405
//! from the router itself rather than a check some future handler could
//! forget.
//!
//! Reads never go through [`ilar::session::SessionStore::load`]: P3
//! measured it failing 86.6% of calls against a live writer, because the
//! checkpoint stamp discipline belongs to the writer. Everything here
//! reads through [`SessionTail`], which is failure-free by construction.
//!
//! ```text
//! GET /api/sessions                                  the head cache
//! GET /api/sessions/{id}?from=&invocation=&limit=     one page, walking back
//! GET /api/sessions/{id}/events?from=&token=          SSE
//! GET /api/sessions/{id}/children
//! GET /api/sessions/{id}/results/{tool_use_id}        full untruncated text
//! GET /api/sessions/{id}/images/{event_id}/{n}        image bytes
//! GET /  /app.css  /app.js                            the page (slice 5)
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
//! Auth: a loopback bind has none — anything that can reach it can read
//! the store directly. Any other bind requires a bearer token, and so
//! does a loopback bind with `ILAR_SERVE_TOKEN` set, because pinning a
//! token is an explicit request for one. Comparison is constant-time;
//! a failure is 401 with an empty body, on every path including
//! unmatched ones, so the token is not an oracle for what exists — the
//! sole exception being the three static assets, which have to load
//! before the page can read the token out of the fragment
//! ([`PUBLIC_PATHS`]).

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
use axum::routing::get;
use axum::{Router, middleware};
use futures::Stream;
use serde::Deserialize;
use serde_json::{Value, json};

use ilar::session::{SessionEvent, SessionStore, SessionTail, TailUpdate};

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
}

pub(crate) fn router(state: ServeState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.css", get(stylesheet))
        .route("/app.js", get(script))
        .route("/api/sessions", get(sessions))
        .route("/api/sessions/{id}", get(transcript))
        .route("/api/sessions/{id}/events", get(events))
        .route("/api/sessions/{id}/children", get(children))
        .route("/api/sessions/{id}/results/{tool_use_id}", get(result_text))
        .route("/api/sessions/{id}/images/{event_id}/{n}", get(image))
        // On the whole router, fallback included: a 404 must not be
        // reachable without the token either.
        .layer(middleware::from_fn_with_state(state.clone(), require_token))
        .with_state(state)
}

// ---------------------------------------------------------------- auth

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
        Some(token) => format!("http://{address}/#token={token}"),
        None => format!("http://{address}/"),
    }
}

/// The three static files, and the one exception to the gate. The token
/// rides in the URL fragment, which a browser never sends upstream — so
/// gating the page that reads that fragment would make the token
/// impossible to deliver, and `--open` would open a 401. These bytes are
/// identical for every install, already public inside the binary, and
/// say nothing whatever about the store; the data behind them stays
/// gated, including the fallback.
const PUBLIC_PATHS: [&str; 3] = ["/", "/app.css", "/app.js"];

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

/// `?token=` for `EventSource`, which cannot set a header. The token is
/// hex, so a percent-decode would have nothing to do.
fn query_token(query: Option<&str>) -> Option<String> {
    query?.split('&').find_map(|pair| {
        pair.strip_prefix("token=")
            .map(std::string::ToString::to_string)
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
            .map(summary)
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
            .map(summary)
            .collect::<Vec<_>>(),
    })))
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
        "session": state.watcher.head(&id).as_ref().map_or(Value::Null, summary),
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

    // A browser resumes with the header; a first connection names its
    // starting line explicitly, usually the transcript's `line`.
    let resume = query.from.or_else(|| last_event_id(&headers));
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
    pending: VecDeque<Event>,
    last_line: usize,
    done: bool,
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
            let message = next_message(&mut self.receiver).await?;
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
/// mtime — and never a lock probe, because acquiring the writer lease to
/// ask would make a read-only server take the one thing it promised not
/// to. `activity` names the tool a working session is running, when it
/// is running one.
fn summary(entry: &SessionEntry) -> Value {
    json!({
        "id": entry.head.id,
        "title": entry.head.title,
        "cwd": entry.head.meta.cwd.as_ref().map(|cwd| cwd.display().to_string()),
        "agent": entry.head.meta.agent,
        "model": entry.head.meta.model,
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

/// The page is three files compiled into the binary: one artifact to
/// ship, and no path a request could traverse. Slice 5 fills them in.
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
    asset(
        "text/javascript; charset=utf-8",
        include_str!("assets/app.js"),
    )
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
