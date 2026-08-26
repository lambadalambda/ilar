//! `ilar serve` over the wire.
//!
//! Every test drives the real binary: a `serve` process on port 0 with
//! its own state directory, a store written by the real `Session`
//! writer, and requests over HTTP. Nothing is stubbed, because the parts
//! that could be stubbed — the tail reader, the projection, the watcher
//! — are the parts already tested in isolation; what is left to prove is
//! that the process wires them together.
//!
//! Two things a test cannot do honestly and how they are handled:
//!
//! - **A non-loopback bind.** Every 127.x address is loopback, so
//!   binding a second one would not exercise auth; binding a real
//!   interface would open a port to the network from a test suite.
//!   `ILAR_SERVE_TOKEN` requires the token on any bind (that is the
//!   documented meaning of pinning one), so the auth test uses it and
//!   exercises the same middleware a `--bind 0.0.0.0` run would. The
//!   bind-to-token decision itself is a unit test in `serve::http`.
//! - **A browser.** `--open` is a call into the existing opener; there
//!   is nothing here to observe.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use ilar::session::{
    ContentBlock, ImageContent, Session, SessionEvent, SessionMeta, SessionStore, Usage, new_id,
};
use serde_json::Value;

/// A `serve` process and the store it is reading.
struct Server {
    child: Child,
    base: String,
    store: SessionStore,
    token: Option<String>,
    client: reqwest::Client,
    _dir: tempfile::TempDir,
}

impl Server {
    fn start(token: Option<&str>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let config = dir.path().join("config");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&config).unwrap();

        let mut command = Command::new(env!("CARGO_BIN_EXE_ilar"));
        command
            .args(["serve", "--bind", "127.0.0.1:0", "--poll-ms", "25"])
            // A hermetic config: serve reads the state directory and
            // must not need anything else — no provider, no key.
            .env("ILAR_STATE_DIR", &state)
            .env("ILAR_CONFIG_DIR", &config)
            .env_remove("ILAR_SERVE_TOKEN")
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(token) = token {
            command.env("ILAR_SERVE_TOKEN", token);
        }
        let mut child = command.spawn().expect("ilar serve starts");
        let url = read_url(child.stdout.take().expect("piped stdout"));

        Self {
            child,
            // The token rides in the fragment; requests take the base.
            base: url
                .split('#')
                .next()
                .unwrap()
                .trim_end_matches('/')
                .to_string(),
            store: SessionStore::new(state.join("sessions")),
            token: token.map(str::to_string),
            client: reqwest::Client::new(),
            _dir: dir,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn request(&self, path: &str) -> reqwest::RequestBuilder {
        let request = self.client.get(self.url(path));
        match &self.token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        self.request(path).send().await.expect("a response")
    }

    async fn json(&self, path: &str) -> Value {
        let response = self.get(path).await;
        assert_eq!(response.status(), 200, "GET {path}");
        response.json().await.expect("JSON")
    }

    /// The listing is fed by a 100 ms directory scan (`--poll-ms 25`),
    /// so a just-created session takes a tick to appear.
    async fn sessions_once_there_are(&self, count: usize) -> Vec<Value> {
        self.sessions_once(
            |sessions| sessions.len() >= count,
            &format!("reached {count} sessions"),
        )
        .await
    }

    /// The listing, once it says what the test is waiting for. Polling a
    /// condition rather than a count: liveness changes without the row
    /// count moving.
    async fn sessions_once(&self, ready: impl Fn(&[Value]) -> bool, expected: &str) -> Vec<Value> {
        for _ in 0..100 {
            let listing = self.json("/api/sessions").await;
            let sessions = listing["sessions"].as_array().unwrap().clone();
            if ready(&sessions) {
                return sessions;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the listing never {expected}");
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The one line the process promises: the URL, printed once, on stdout.
fn read_url(stdout: std::process::ChildStdout) -> String {
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    for _ in 0..10 {
        line.clear();
        let read = reader.read_line(&mut line).expect("stdout");
        assert_ne!(read, 0, "ilar serve exited before printing a URL");
        if line.starts_with("http://") {
            return line.trim().to_string();
        }
    }
    panic!("no URL on stdout");
}

// ------------------------------------------------------------ fixtures

fn start_session(store: &SessionStore, cwd: &str) -> (String, Session) {
    let id = new_id();
    let session = store
        .create(SessionMeta {
            session_id: id.clone(),
            parent_id: None,
            agent: "build".into(),
            model: "zai/glm-4.7".into(),
            workspace: None,
            cwd: Some(PathBuf::from(cwd)),
        })
        .unwrap();
    (id, session)
}

fn start_child(store: &SessionStore, parent_id: &str) -> (String, Session) {
    let id = new_id();
    let session = store
        .create(SessionMeta {
            session_id: id.clone(),
            parent_id: Some(parent_id.to_string()),
            agent: "explore".into(),
            model: "zai/glm-4.7".into(),
            workspace: None,
            cwd: None,
        })
        .unwrap();
    (id, session)
}

fn user(text: &str) -> SessionEvent {
    SessionEvent::UserMessage {
        id: new_id(),
        text: text.into(),
        images: Vec::new(),
        ts: chrono::Utc::now(),
    }
}

fn assistant(text: &str) -> SessionEvent {
    SessionEvent::AssistantMessage {
        id: new_id(),
        model: "zai/glm-4.7".into(),
        content: vec![ContentBlock::Text { text: text.into() }],
        usage: Usage {
            input_tokens: 10,
            output_tokens: 5,
            ..Usage::default()
        },
        stop_reason: "end_turn".into(),
        ts: chrono::Utc::now(),
    }
}

// ----------------------------------------------------------- SSE reader

struct Frames {
    response: reqwest::Response,
    buffer: String,
}

#[derive(Debug)]
struct Frame {
    id: Option<String>,
    event: String,
    data: Value,
}

impl Frames {
    async fn open(server: &Server, path: &str) -> Self {
        let response = server.get(path).await;
        assert_eq!(response.status(), 200, "GET {path}");
        Self {
            response,
            buffer: String::new(),
        }
    }

    async fn open_at(server: &Server, path: &str, last_event_id: usize) -> Self {
        let response = server
            .request(path)
            .header("last-event-id", last_event_id.to_string())
            .send()
            .await
            .expect("a response");
        assert_eq!(response.status(), 200);
        Self {
            response,
            buffer: String::new(),
        }
    }

    /// The next data frame. Keep-alive comments are not frames.
    async fn next(&mut self) -> Frame {
        loop {
            if let Some(end) = self.buffer.find("\n\n") {
                let block = self.buffer[..end].to_string();
                self.buffer.drain(..end + 2);
                if let Some(frame) = parse_frame(&block) {
                    return frame;
                }
                continue;
            }
            let chunk = tokio::time::timeout(Duration::from_secs(10), self.response.chunk())
                .await
                .expect("an SSE frame within ten seconds")
                .expect("a readable stream")
                .expect("the stream did not end");
            self.buffer.push_str(std::str::from_utf8(&chunk).unwrap());
        }
    }
}

fn parse_frame(block: &str) -> Option<Frame> {
    let (mut id, mut event, mut data) = (None, None, None);
    for line in block.lines() {
        match line.split_once(": ").or_else(|| line.split_once(':')) {
            Some(("id", value)) => id = Some(value.trim().to_string()),
            Some(("event", value)) => event = Some(value.trim().to_string()),
            Some(("data", value)) => data = Some(serde_json::from_str(value.trim()).unwrap()),
            // A keep-alive comment, or a field this client ignores.
            _ => {}
        }
    }
    Some(Frame {
        id,
        event: event?,
        data: data.unwrap_or(Value::Null),
    })
}

// -------------------------------------------------------------- tests

/// The listing is what the page groups by directory, so every field the
/// grouping and the rows need has to survive the trip.
#[tokio::test]
async fn the_listing_carries_a_row_per_root_session() {
    let server = Server::start(None);
    let (first, mut first_session) = start_session(&server.store, "/tmp/alpha");
    first_session
        .append(user("what does the watcher do?"))
        .unwrap();
    let (second, mut second_session) = start_session(&server.store, "/tmp/beta");
    second_session.append(user("second question")).unwrap();
    let (child, mut child_session) = start_child(&server.store, &first);
    child_session.append(user("review this")).unwrap();

    let sessions = server.sessions_once_there_are(2).await;
    assert_eq!(sessions.len(), 2, "the subagent log is not a row");
    let row = sessions
        .iter()
        .find(|row| row["id"] == first.as_str())
        .expect("the first session");
    assert_eq!(row["title"], "what does the watcher do?");
    assert_eq!(row["cwd"], "/tmp/alpha", "the page groups by this");
    assert_eq!(row["agent"], "build");
    assert_eq!(row["model"], "zai/glm-4.7");
    assert_eq!(
        row["state"], "idle",
        "written a moment ago, but not running"
    );
    assert_eq!(row["activity"], Value::Null);
    assert!(row["modified"].as_str().unwrap().contains('T'), "RFC 3339");
    assert!(
        sessions
            .iter()
            .any(|row| row["cwd"] == "/tmp/beta" && row["id"] == second.as_str())
    );

    // The other half of the same cache.
    let children = server
        .json(&format!("/api/sessions/{first}/children"))
        .await;
    let children = children["children"].as_array().unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0]["id"], child.as_str());
    assert_eq!(children[0]["agent"], "explore");
    assert_eq!(children[0]["parent_id"], first.as_str());
}

/// A transcript is read newest-first and paged backwards, because P7's
/// worst session is 906 events and nobody scrolls from the top.
#[tokio::test]
async fn a_transcript_pages_backwards_to_its_beginning() {
    let server = Server::start(None);
    let (id, mut session) = start_session(&server.store, "/tmp/alpha");
    for turn in 0..6 {
        session.append(user(&format!("question {turn}"))).unwrap();
        session
            .append(assistant(&format!("answer {turn}")))
            .unwrap();
    }

    let page = server.json(&format!("/api/sessions/{id}?limit=5")).await;
    assert_eq!(page["count"], 13, "one meta line and twelve turns");
    assert_eq!(page["line"], 13, "the physical line an SSE resumes from");
    assert_eq!(page["cursor"], 8);
    assert_eq!(page["has_more"], true);
    assert_eq!(page["session"]["title"], "question 0");
    assert_eq!(page["usage"]["input"], 60, "totals cover the whole session");
    let events = page["events"].as_array().unwrap();
    assert_eq!(events.len(), 5);
    assert_eq!(events[4]["type"], "assistant_message");
    assert_eq!(events[4]["content"][0]["text"], "answer 5");

    let page = server
        .json(&format!("/api/sessions/{id}?from=8&limit=5"))
        .await;
    assert_eq!(page["cursor"], 3);
    assert_eq!(page["has_more"], true);
    assert_eq!(page["events"][0]["text"], "question 1");

    let page = server
        .json(&format!("/api/sessions/{id}?from=3&limit=5"))
        .await;
    assert_eq!(page["cursor"], 0);
    assert_eq!(page["has_more"], false);
    assert_eq!(page["events"].as_array().unwrap().len(), 3);
    assert_eq!(page["events"][0]["type"], "meta", "back at the first line");

    // A session that is not there, and an id that could not be one.
    assert_eq!(
        server
            .get(&format!("/api/sessions/{}", new_id()))
            .await
            .status(),
        404
    );
    assert_eq!(server.get("/api/sessions/not-a-uuid").await.status(), 400);
}

/// One subagent turn, sliced out of the child's own log — the link the
/// listing offers instead of inlining a nested transcript.
#[tokio::test]
async fn a_child_transcript_can_be_narrowed_to_one_invocation() {
    let server = Server::start(None);
    let (parent, _parent_session) = start_session(&server.store, "/tmp/alpha");
    let (child, mut session) = start_child(&server.store, &parent);
    session
        .append(SessionEvent::SubagentInvocation {
            id: new_id(),
            parent_tool_call_id: "task-1".into(),
            ts: chrono::Utc::now(),
        })
        .unwrap();
    session.append(user("first task")).unwrap();
    session
        .append(SessionEvent::SubagentInvocation {
            id: new_id(),
            parent_tool_call_id: "task-2".into(),
            ts: chrono::Utc::now(),
        })
        .unwrap();
    session.append(user("second task")).unwrap();

    let page = server
        .json(&format!("/api/sessions/{child}?invocation=task-1"))
        .await;
    assert_eq!(page["count"], 1);
    assert_eq!(page["events"][0]["text"], "first task");

    let page = server
        .json(&format!("/api/sessions/{child}?invocation=task-9"))
        .await;
    assert_eq!(page["count"], 0);
}

/// The point of the whole slice: a line appended after the connection is
/// open arrives without anyone asking again.
#[tokio::test]
async fn the_event_stream_delivers_an_append_made_after_it_opened() {
    let server = Server::start(None);
    let (id, mut session) = start_session(&server.store, "/tmp/alpha");

    let mut frames = Frames::open(&server, &format!("/api/sessions/{id}/events")).await;
    session.append(user("live question")).unwrap();

    let frame = frames.next().await;
    assert_eq!(frame.event, "append");
    assert_eq!(
        frame.id.as_deref(),
        Some("2"),
        "the SSE id is the file line"
    );
    assert_eq!(frame.data["line"], 2);
    assert_eq!(frame.data["event"]["type"], "user_message");
    assert_eq!(frame.data["event"]["text"], "live question");

    session.append(assistant("live answer")).unwrap();
    let frame = frames.next().await;
    assert_eq!(frame.id.as_deref(), Some("3"));
    assert_eq!(frame.data["event"]["content"][0]["text"], "live answer");
}

/// Phase 3's point: a turn's tokens reach the browser before the step
/// that produced them is committed. The scratch here is written by the
/// same core writer `run_turn` uses, so this is the real format on the
/// real wire — only the turn around it is missing.
#[tokio::test]
async fn a_live_turn_streams_delta_frames_before_its_step_commits() {
    let server = Server::start(None);
    let (id, mut session) = start_session(&server.store, "/tmp/alpha");
    let mut frames = Frames::open(&server, &format!("/api/sessions/{id}/events")).await;

    let mut live = ilar::session::LiveScratch::start(&server.store, &id);
    live.tool_started("bash-1", "bash", "cargo test");
    let frame = frames.next().await;
    assert_eq!(frame.event, "delta");
    assert_eq!(
        frame.id, None,
        "ephemeral: a scratch line is not a line of the log, so it never enters Last-Event-ID"
    );
    assert_eq!(frame.data["type"], "tool_started");
    assert_eq!(frame.data["name"], "bash");
    assert_eq!(frame.data["summary"], "cargo test");

    // And the listing says the same thing about the same turn.
    let row = server
        .sessions_once(
            |sessions| sessions.iter().any(|row| row["state"] == "working"),
            "showed a working session",
        )
        .await
        .into_iter()
        .find(|row| row["id"] == id.as_str())
        .expect("the session");
    assert_eq!(row["state"], "working");
    assert_eq!(row["activity"], "bash: cargo test");

    // A second connection, opened in the middle of the same step: it is
    // handed the row as it already stands rather than starting from
    // whatever the turn says next.
    let mut joined = Frames::open(&server, &format!("/api/sessions/{id}/events")).await;
    let frame = joined.next().await;
    assert_eq!(frame.event, "delta");
    assert_eq!(frame.data["type"], "tool_started");
    assert_eq!(frame.data["summary"], "cargo test");
    drop(joined);

    // The step commits: the committed event arrives on the main stream,
    // and the scratch's reset retires the stand-in that preceded it.
    session.append(assistant("all done")).unwrap();
    live.commit();
    let mut seen: Vec<(String, Value)> = Vec::new();
    while seen.len() < 2 {
        let frame = frames.next().await;
        seen.push((frame.event, frame.data));
    }
    assert!(
        seen.iter()
            .any(|(event, data)| event == "append" && data["event"]["type"] == "assistant_message"),
        "{seen:?}"
    );
    assert!(
        seen.iter()
            .any(|(event, data)| event == "delta" && data["type"] == "reset"),
        "{seen:?}"
    );

    // The turn ends with the scratch, and so does the streaming row.
    drop(live);
    let frame = frames.next().await;
    assert_eq!(frame.event, "delta");
    assert_eq!(frame.data["type"], "reset");
}

/// Reconnecting is a re-read and a skip: the client names the last line
/// it folded, and gets what followed — once.
#[tokio::test]
async fn last_event_id_resumes_without_duplicates() {
    let server = Server::start(None);
    let (id, mut session) = start_session(&server.store, "/tmp/alpha");
    session.append(user("first")).unwrap();
    session.append(assistant("did first")).unwrap();

    // A first connection, resuming from the meta line.
    let mut frames = Frames::open(&server, &format!("/api/sessions/{id}/events?from=1")).await;
    assert_eq!(frames.next().await.id.as_deref(), Some("2"));
    assert_eq!(frames.next().await.id.as_deref(), Some("3"));
    session.append(user("second")).unwrap();
    assert_eq!(frames.next().await.id.as_deref(), Some("4"));
    drop(frames);

    // A reconnection that missed line 5, which landed while it was gone.
    session.append(assistant("did second")).unwrap();
    let mut frames = Frames::open_at(&server, &format!("/api/sessions/{id}/events"), 4).await;
    let resumed = frames.next().await;
    assert_eq!(resumed.id.as_deref(), Some("5"), "no line 4 again");
    assert_eq!(resumed.data["event"]["content"][0]["text"], "did second");

    session.append(user("third")).unwrap();
    let frame = frames.next().await;
    assert_eq!(frame.id.as_deref(), Some("6"));
    assert_eq!(
        frame.data["event"]["text"], "third",
        "the live edge follows the catch-up with no seam"
    );
}

/// A deleted session is terminal on the wire too.
#[tokio::test]
async fn deleting_a_session_ends_its_stream() {
    let server = Server::start(None);
    let (id, session) = start_session(&server.store, "/tmp/alpha");
    drop(session);

    let mut frames = Frames::open(&server, &format!("/api/sessions/{id}/events")).await;
    server.store.delete(&id).unwrap();
    assert_eq!(frames.next().await.event, "deleted");
}

/// The projection deliberately drops bulk: the full text and the image
/// bytes are routes, and this is the round trip.
#[tokio::test]
async fn the_result_and_image_routes_return_what_the_projection_left_out() {
    let server = Server::start(None);
    let (id, mut session) = start_session(&server.store, "/tmp/alpha");
    let pixels: Vec<u8> = (0..=255u8).cycle().take(4_096).collect();
    let long = "x".repeat(ilar::text::MAX_DETAIL_CHARS * 2);
    let event_id = new_id();
    session
        .append(SessionEvent::ToolResult {
            id: event_id.clone(),
            tool_use_id: "bash-1".into(),
            content: long.clone(),
            is_error: false,
            images: vec![ImageContent::png(&pixels)],
            child_session_id: None,
            state: None,
            ts: chrono::Utc::now(),
        })
        .unwrap();

    let page = server.json(&format!("/api/sessions/{id}")).await;
    let result = &page["events"][1];
    assert_eq!(result["truncated"], true);
    assert!(result["text"].as_str().unwrap().len() < long.len());
    assert_eq!(result["images"][0]["media_type"], "image/png");
    // `ImageContent::byte_len` estimates from the base64 length, so the
    // descriptor is a size to render next to, not a content length.
    let described = result["images"][0]["bytes"].as_i64().unwrap();
    assert!((described - 4_096).abs() <= 3, "{described}");

    let response = server
        .get(&format!("/api/sessions/{id}/results/bash-1"))
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["content-type"],
        "text/plain; charset=utf-8"
    );
    assert_eq!(response.text().await.unwrap(), long, "untruncated");

    let response = server
        .get(&format!("/api/sessions/{id}/images/{event_id}/0"))
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "image/png");
    assert_eq!(response.bytes().await.unwrap().as_ref(), pixels.as_slice());

    assert_eq!(
        server
            .get(&format!("/api/sessions/{id}/images/{event_id}/7"))
            .await
            .status(),
        404
    );
    assert_eq!(
        server
            .get(&format!("/api/sessions/{id}/results/no-such-call"))
            .await
            .status(),
        404
    );
}

/// Phase 2 is read-only, and the router is where that is enforced: no
/// handler exists for any other method, so none can be forgotten.
#[tokio::test]
async fn the_router_serves_the_page_and_refuses_every_other_method() {
    let server = Server::start(None);

    // The three assets are compiled in, so serving them is the whole
    // deployment story: a body with something in it, under the type the
    // browser needs to treat it as a page rather than a download.
    for (path, content_type) in [
        ("/", "text/html; charset=utf-8"),
        ("/app.css", "text/css; charset=utf-8"),
        ("/app.js", "text/javascript; charset=utf-8"),
    ] {
        let response = server.get(path).await;
        assert_eq!(response.status(), 200, "GET {path}");
        assert_eq!(response.headers()["content-type"], content_type, "{path}");
        let body = response.text().await.unwrap();
        assert!(!body.trim().is_empty(), "{path} is empty");
    }

    // The page loads its own two files and nothing else: no CDN, no
    // webfont, no build step — `ilar serve` works on a plane.
    let index = server.get("/").await.text().await.unwrap();
    assert!(index.contains("href=\"/app.css\""), "{index}");
    assert!(index.contains("src=\"/app.js\""), "{index}");
    assert!(!index.contains("http"), "no external URL: {index}");

    for method in [reqwest::Method::POST, reqwest::Method::DELETE] {
        let response = server
            .client
            .request(method.clone(), server.url("/api/sessions"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 405, "{method} /api/sessions");
    }
    assert_eq!(server.get("/nope").await.status(), 404);
}

/// With a token required, every route needs it — including the SSE
/// route, which carries it in the query because `EventSource` cannot
/// set a header.
#[tokio::test]
async fn a_required_token_gates_every_route() {
    let server = Server::start(Some("s3cret-token"));
    let (id, mut session) = start_session(&server.store, "/tmp/alpha");
    session.append(user("private")).unwrap();

    let response = server
        .client
        .get(server.url("/api/sessions"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401, "no token");
    assert!(response.bytes().await.unwrap().is_empty(), "and no body");

    let response = server
        .client
        .get(server.url("/api/sessions"))
        .bearer_auth("not-the-token")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401, "wrong token");

    // 401 before 404: the token is not an oracle for what exists.
    let response = server.client.get(server.url("/nope")).send().await.unwrap();
    assert_eq!(response.status(), 401);

    // The page is the exception, and has to be: the token arrives in the
    // URL fragment, which a browser never sends, so a gated `/` would
    // make the printed URL impossible to open. The three assets carry
    // nothing from the store.
    for path in ["/", "/app.css", "/app.js"] {
        let response = server.client.get(server.url(path)).send().await.unwrap();
        assert_eq!(response.status(), 200, "un-tokened GET {path}");
    }

    // The right token, in the header and in the query.
    assert_eq!(server.get("/api/sessions").await.status(), 200);
    let sessions = server.sessions_once_there_are(1).await;
    assert_eq!(sessions[0]["id"], id.as_str());

    let response = server
        .client
        .get(server.url(&format!("/api/sessions/{id}/events?token=s3cret-token")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "EventSource passes ?token=");
    let response = server
        .client
        .get(server.url(&format!("/api/sessions/{id}/events?token=wrong")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
}

/// Serving needs the state directory and nothing else: no provider, no
/// key, no model — the config here is an empty directory.
#[tokio::test]
async fn serving_needs_no_provider_configuration() {
    let server = Server::start(None);
    assert!(
        server.base.starts_with("http://127.0.0.1:"),
        "{}",
        server.base
    );
    // An empty store — the session directory does not even exist yet —
    // answered by a process that never read a provider or a key.
    assert!(
        server.json("/api/sessions").await["sessions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}
