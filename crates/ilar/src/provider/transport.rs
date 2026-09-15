use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::task::{Context, Poll};

use futures::{FutureExt, Stream, StreamExt};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use super::sse::SseParser;
use super::{EventStream, ProviderEvent};

/// Bound on TCP/TLS connection setup.
pub(super) const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// Idle bound between reads (awaiting headers or the next stream chunk).
/// Streams deliberately have NO total deadline: reasoning models
/// legitimately generate for 10+ minutes (glm-5.3 was killed at exactly
/// the old 600s total cap after 117KB of healthy thinking). A live stream
/// keeps delivering deltas; a dead connection trips this instead.
pub(super) const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Streaming HTTP client: connect + idle timeouts only, by design.
/// Headers that tie a request to its conversation, for the backends
/// that key on them. The value is the request's `cache_key` — the
/// session id — which is what the same backends already see in
/// `prompt_cache_key`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Affinity {
    /// The public OpenAI API, z.ai and local servers: nothing extra.
    None,
    /// The Codex backend pins a request to the shard holding its cached
    /// prefix by `session-id`/`thread-id`; `prompt_cache_key` alone does
    /// not (measured: 2/10 follow-up steps read a cache without these,
    /// 10/10 with them).
    Codex,
    /// The OpenCode gateways log `x-opencode-session` (and refuse its
    /// absence from 2026-09-06), the client name, and the user agent
    /// reqwest otherwise leaves blank.
    OpenCode,
}

impl Affinity {
    pub(super) fn headers(self, cache_key: Option<&str>) -> Vec<(&'static str, String)> {
        match self {
            Self::None => Vec::new(),
            Self::Codex => cache_key
                .map(|key| {
                    vec![
                        ("session-id", key.to_string()),
                        ("thread-id", key.to_string()),
                    ]
                })
                .unwrap_or_default(),
            Self::OpenCode => vec![
                // A request outside any session (topic naming) still
                // names one, so nothing goes out anonymous.
                (
                    "x-opencode-session",
                    cache_key.map_or_else(process_session, str::to_string),
                ),
                ("x-opencode-client", "ilar".to_string()),
                ("user-agent", format!("ilar/{}", env!("CARGO_PKG_VERSION"))),
            ],
        }
    }
}

/// The session a request outside every session is attributed to: one
/// per process, so a gateway sees one caller rather than a new one per
/// request.
fn process_session() -> String {
    format!("ilar-process-{}", std::process::id())
}

pub(super) fn streaming_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(IDLE_TIMEOUT)
        .build()
        .expect("valid provider HTTP client")
}

/// Anthropic's `overloaded_error` status. `StatusCode` has no named
/// constant for it, so it is compared numerically.
const OVERLOADED: u16 = 529;

/// A server that is throttling or full: 429, and Anthropic's 529. These
/// are retryable like the rest, but the loop gives them a longer budget.
fn rate_limited_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.as_u16() == OVERLOADED
}

/// Longest wait a server's `Retry-After` can ask for before it is
/// treated as "come back later" rather than "wait here".
const RETRY_AFTER_CAP: std::time::Duration = std::time::Duration::from_secs(300);

/// The server's `Retry-After`, in the delay-seconds form. The HTTP-date
/// form is legal but no provider ilar talks to uses it; it reads as
/// absent, and the loop's own backoff applies.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<std::time::Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(|seconds| std::time::Duration::from_secs(seconds).min(RETRY_AFTER_CAP))
}

fn retryable_status(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::REQUEST_TIMEOUT
            | reqwest::StatusCode::CONFLICT
            | reqwest::StatusCode::TOO_MANY_REQUESTS
            | reqwest::StatusCode::INTERNAL_SERVER_ERROR
            | reqwest::StatusCode::BAD_GATEWAY
            | reqwest::StatusCode::SERVICE_UNAVAILABLE
            | reqwest::StatusCode::GATEWAY_TIMEOUT
    ) || status.as_u16() == OVERLOADED
}

/// Full error chain — reqwest's Display alone hides the cause ("error
/// decoding response body" for what is actually a timeout or reset).
fn error_with_sources(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(current) = source {
        message.push_str(": ");
        message.push_str(&current.to_string());
        source = current.source();
    }
    message
}

pub(super) struct TransportResponse {
    pub response: reqwest::Response,
    pub secrets: Vec<String>,
    /// Who answered, for an error that has to say whose credential was
    /// refused. The model id is not enough: one key can serve two
    /// gateways and one gateway can serve many models.
    pub provider: &'static str,
    /// What the request authenticated with, because the two are fixed
    /// in different places: a key by a variable or the TOML, stored
    /// OAuth tokens by `ilar login`.
    pub credential: Credential,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Credential {
    /// An API key, or no credential at all (a local server).
    ApiKey,
    /// Tokens from the auth store, refreshed on the way out.
    OAuth,
}

pub(super) enum TransportError {
    Retryable(String),
    Fatal(String),
}

pub(super) fn retryable(error: impl ToString) -> TransportError {
    TransportError::Retryable(error.to_string())
}

/// The full chain, not reqwest's Display: "error sending request for
/// url (…)" is the same line for a DNS failure, a refused connection
/// and a TLS mismatch, and the cause is what says which.
pub(super) fn request_error(error: reqwest::Error) -> TransportError {
    let message = error_with_sources(&error);
    if error.is_connect() || error.is_timeout() || error.is_body() {
        retryable(message)
    } else {
        fatal(message)
    }
}

pub(super) fn fatal(error: impl ToString) -> TransportError {
    TransportError::Fatal(error.to_string())
}

pub(super) trait EventMapper: Send + 'static {
    fn map(&mut self, data: &str) -> Result<Vec<ProviderEvent>, String>;
    fn finish(&mut self) -> Option<ProviderEvent>;
}

pub(super) fn stream<F, M>(send: F, mut mapper: M) -> EventStream
where
    F: Future<Output = Result<TransportResponse, TransportError>> + Send + 'static,
    M: EventMapper,
{
    let (tx, rx) = mpsc::channel(64);
    let tx_panic = tx.clone();
    let pump = async move {
        let TransportResponse {
            response,
            secrets,
            provider,
            credential,
        } = match send.await {
            Ok(response) => response,
            Err(TransportError::Retryable(error)) => {
                let _ = tx.send(ProviderEvent::RetryableError(error)).await;
                return;
            }
            Err(TransportError::Fatal(error)) => {
                let _ = tx.send(ProviderEvent::Error(error)).await;
                return;
            }
        };
        if !response.status().is_success() {
            let status = response.status();
            let retry_after = retry_after(response.headers());
            let secret_refs = secrets.iter().map(String::as_str).collect::<Vec<_>>();
            let body = super::error_body::bounded_error_body(response, &secret_refs).await;
            let event = if rate_limited_status(status) {
                ProviderEvent::RateLimited {
                    message: format!("HTTP {status}: {body}"),
                    retry_after,
                }
            } else if retryable_status(status) {
                ProviderEvent::RetryableError(format!("HTTP {status}: {body}"))
            } else {
                ProviderEvent::Error(status_error(provider, credential, status, &body))
            };
            let _ = tx.send(event).await;
            return;
        }

        let mut parser = SseParser::new();
        let mut bytes = response.bytes_stream();
        while let Some(chunk) = bytes.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    let _ = tx
                        .send(ProviderEvent::RetryableError(error_with_sources(&error)))
                        .await;
                    return;
                }
            };
            let data = match parser.feed(&chunk) {
                Ok(data) => data,
                Err(error) => {
                    let _ = tx.send(ProviderEvent::Error(error.to_string())).await;
                    return;
                }
            };
            for data in data {
                let events = match mapper.map(&data) {
                    Ok(events) => events,
                    Err(error) => {
                        // Include the offending wire event so decode
                        // failures are diagnosable from the session alone.
                        let _ = tx
                            .send(ProviderEvent::Error(decode_error(error, &data, &secrets)))
                            .await;
                        return;
                    }
                };
                for event in events {
                    let terminal = is_terminal(&event);
                    if tx.send(event).await.is_err() || terminal {
                        return;
                    }
                }
            }
        }
        if let Err(error) = parser.finish() {
            let _ = tx.send(ProviderEvent::Error(error.to_string())).await;
            return;
        }
        if let Some(event) = mapper.finish() {
            let _ = tx.send(event).await;
        }
    };
    let handle = tokio::spawn(async move {
        if let Err(panic) = AssertUnwindSafe(pump).catch_unwind().await {
            let message = panic
                .downcast_ref::<&str>()
                .map(|message| message.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "provider pump panicked".into());
            let _ = tx_panic
                .send(ProviderEvent::Error(format!("internal error: {message}")))
                .await;
        }
    });

    Box::pin(AbortOnDropStream {
        stream: ReceiverStream::new(rx),
        handle: Some(handle),
    })
}

/// A failed request as a line someone can act on. A refused credential
/// is the one status whose body says nothing useful — `{"error":
/// {"code":"1002",…}}` names neither the key nor where it came from —
/// so it gets a lead line and keeps the body underneath, where the
/// provider's own reason is.
fn status_error(
    provider: &str,
    credential: Credential,
    status: reqwest::StatusCode,
    body: &str,
) -> String {
    match status {
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => {
            let next_step = match credential {
                // The refresh has already been tried once by the time a
                // 401 gets here, so the sign-in itself is what is left.
                Credential::OAuth => "run `ilar login` to sign in again".to_string(),
                Credential::ApiKey => {
                    format!("check {}", crate::config::credential_sources(provider))
                }
            };
            let what = match credential {
                Credential::OAuth => "the stored ChatGPT tokens",
                Credential::ApiKey => "the credential",
            };
            format!("{provider} rejected {what} (HTTP {status}): {next_step}\n{body}")
        }
        _ => format!("HTTP {status}: {body}"),
    }
}

const MAX_EVENT_SNIPPET_CHARS: usize = 600;

/// Decode error annotated with a bounded, secret-scrubbed snippet of the
/// SSE event that failed to map.
fn decode_error(error: String, data: &str, secrets: &[String]) -> String {
    let mut snippet: String = data.chars().take(MAX_EVENT_SNIPPET_CHARS).collect();
    if data.chars().count() > MAX_EVENT_SNIPPET_CHARS {
        snippet.push('…');
    }
    for secret in secrets {
        if !secret.is_empty() {
            snippet = snippet.replace(secret, "<redacted>");
        }
    }
    format!("{error} · offending event: {snippet}")
}

fn is_terminal(event: &ProviderEvent) -> bool {
    matches!(
        event,
        ProviderEvent::TurnComplete { .. }
            | ProviderEvent::Error(_)
            | ProviderEvent::RetryableError(_)
            | ProviderEvent::RateLimited { .. }
    )
}

struct AbortOnDropStream<S> {
    stream: S,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl<S> Drop for AbortOnDropStream<S> {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

impl<S: Stream + Unpin> Stream for AbortOnDropStream<S> {
    type Item = S::Item;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<S::Item>> {
        self.stream.poll_next_unpin(cx)
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;

    use super::*;

    #[derive(Default)]
    struct TextMapper;

    impl EventMapper for TextMapper {
        fn map(&mut self, data: &str) -> Result<Vec<ProviderEvent>, String> {
            Ok(vec![ProviderEvent::TextDelta(data.to_string())])
        }

        fn finish(&mut self) -> Option<ProviderEvent> {
            None
        }
    }

    struct TerminalMapper;

    impl EventMapper for TerminalMapper {
        fn map(&mut self, data: &str) -> Result<Vec<ProviderEvent>, String> {
            if data == "done" {
                Ok(vec![ProviderEvent::TurnComplete {
                    stop_reason: super::super::StopReason::EndTurn,
                    usage: crate::session::Usage::default(),
                }])
            } else {
                Ok(vec![ProviderEvent::TextDelta(data.to_string())])
            }
        }

        fn finish(&mut self) -> Option<ProviderEvent> {
            Some(ProviderEvent::Error("unexpected EOF".into()))
        }
    }

    async fn response(body: &str) -> reqwest::Response {
        status_response("200 OK", body).await
    }

    async fn status_response(status: &str, body: &str) -> reqwest::Response {
        status_response_with(status, "", body).await
    }

    /// `extra` is raw header lines, each `\r\n`-terminated.
    async fn status_response_with(status: &str, extra: &str, body: &str) -> reqwest::Response {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let status = status.to_string();
        let extra = extra.to_string();
        let body = body.to_string();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            let _ = socket.read(&mut request).await.unwrap();
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 {status}\r\ncontent-type: text/event-stream\r\n{extra}content-length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        reqwest::get(format!("http://{address}")).await.unwrap()
    }

    #[test]
    fn only_transient_http_statuses_are_retryable() {
        for status in [
            reqwest::StatusCode::REQUEST_TIMEOUT,
            reqwest::StatusCode::CONFLICT,
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            reqwest::StatusCode::BAD_GATEWAY,
            reqwest::StatusCode::from_u16(OVERLOADED).unwrap(),
        ] {
            assert!(retryable_status(status), "{status}");
        }
        for status in [
            reqwest::StatusCode::BAD_REQUEST,
            reqwest::StatusCode::UNAUTHORIZED,
            reqwest::StatusCode::FORBIDDEN,
            reqwest::StatusCode::NOT_FOUND,
            reqwest::StatusCode::NOT_IMPLEMENTED,
            reqwest::StatusCode::HTTP_VERSION_NOT_SUPPORTED,
        ] {
            assert!(!retryable_status(status), "{status}");
        }
    }

    #[tokio::test]
    async fn overloaded_responses_are_retryable_stream_errors() {
        let response = status_response("529 Overloaded", "{\"type\":\"overloaded_error\"}").await;
        let events = stream(
            async {
                Ok(TransportResponse {
                    response,
                    secrets: Vec::new(),
                    provider: "zai",
                    credential: Credential::ApiKey,
                })
            },
            TextMapper,
        )
        .collect::<Vec<_>>()
        .await;

        let [
            ProviderEvent::RateLimited {
                message,
                retry_after: None,
            },
        ] = events.as_slice()
        else {
            panic!("expected a single rate-limit error: {events:?}");
        };
        assert!(message.contains("overloaded_error"), "{message}");
    }

    /// A 429 is a rate limit, and the server's `Retry-After` rides along
    /// when it sends one; a 503 stays an ordinary retryable error.
    #[tokio::test]
    async fn rate_limits_carry_the_servers_retry_after() {
        let collect = |response: reqwest::Response| {
            stream(
                async {
                    Ok(TransportResponse {
                        response,
                        secrets: Vec::new(),
                        provider: "zai",
                        credential: Credential::ApiKey,
                    })
                },
                TextMapper,
            )
            .collect::<Vec<_>>()
        };
        let hinted = status_response_with(
            "429 Too Many Requests",
            "retry-after: 7\r\n",
            "{\"error\":{\"code\":\"rate_limit_exceeded\"}}",
        )
        .await;
        let events = collect(hinted).await;
        assert!(
            matches!(
                events.as_slice(),
                [ProviderEvent::RateLimited { message, retry_after: Some(after) }]
                    if message.contains("rate_limit_exceeded")
                        && *after == std::time::Duration::from_secs(7)
            ),
            "{events:?}"
        );

        let bare = status_response("429 Too Many Requests", "slow down").await;
        let events = collect(bare).await;
        assert!(
            matches!(
                events.as_slice(),
                [ProviderEvent::RateLimited {
                    retry_after: None,
                    ..
                }]
            ),
            "{events:?}"
        );

        // A date is legal but unread; a huge number is clamped.
        let dated = status_response_with(
            "429 Too Many Requests",
            "retry-after: Wed, 21 Oct 2026 07:28:00 GMT\r\n",
            "later",
        )
        .await;
        let events = collect(dated).await;
        assert!(
            matches!(
                events.as_slice(),
                [ProviderEvent::RateLimited {
                    retry_after: None,
                    ..
                }]
            ),
            "{events:?}"
        );
        let far =
            status_response_with("429 Too Many Requests", "retry-after: 86400\r\n", "later").await;
        let events = collect(far).await;
        assert!(
            matches!(
                events.as_slice(),
                [ProviderEvent::RateLimited { retry_after: Some(after), .. }]
                    if *after == RETRY_AFTER_CAP
            ),
            "{events:?}"
        );

        let unavailable = status_response("503 Service Unavailable", "down").await;
        let events = collect(unavailable).await;
        assert!(
            matches!(events.as_slice(), [ProviderEvent::RetryableError(_)]),
            "{events:?}"
        );
    }

    /// A refused credential used to arrive as the provider's raw JSON.
    /// The lead line says whose key it was and where that key is
    /// configured; the body stays, since it is the only place the
    /// provider's own reason appears.
    #[tokio::test]
    async fn a_refused_credential_leads_with_the_key_to_check() {
        let response = status_response(
            "401 Unauthorized",
            "{\"error\":{\"code\":\"1002\",\"message\":\"invalid token\"}}",
        )
        .await;
        let events = stream(
            async {
                Ok(TransportResponse {
                    response,
                    secrets: Vec::new(),
                    provider: "zai",
                    credential: Credential::ApiKey,
                })
            },
            TextMapper,
        )
        .collect::<Vec<_>>()
        .await;
        let [ProviderEvent::Error(error)] = events.as_slice() else {
            panic!("expected a single terminal error: {events:?}");
        };
        let lead = error.lines().next().unwrap_or_default();
        assert!(lead.contains("zai rejected the credential"), "{error}");
        assert!(lead.contains("HTTP 401"), "{error}");
        assert!(lead.contains("ILAR_ZAI_API_KEY"), "{error}");
        assert!(lead.contains("providers.zai.api_key"), "{error}");
        assert!(error.contains("\"1002\""), "{error}");

        // Every other fatal status keeps the plain form.
        assert_eq!(
            status_error(
                "zai",
                Credential::ApiKey,
                reqwest::StatusCode::BAD_REQUEST,
                "bad"
            ),
            "HTTP 400 Bad Request: bad"
        );

        // OAuth is fixed somewhere else entirely: naming a key
        // variable to somebody signed in with a ChatGPT account sends
        // them after a key they never had.
        let oauth = status_error(
            "openai",
            Credential::OAuth,
            reqwest::StatusCode::UNAUTHORIZED,
            "{}",
        );
        assert!(oauth.contains("ilar login"), "{oauth}");
        assert!(!oauth.contains("ILAR_OPENAI_API_KEY"), "{oauth}");
    }

    #[tokio::test]
    async fn request_errors_retry_connections_but_not_invalid_requests() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let connection = reqwest::Client::new()
            .get(format!("http://{address}"))
            .send()
            .await
            .unwrap_err();
        // The cause rides along: reqwest's own Display is the same
        // "error sending request" line for a refused connection, a DNS
        // failure and a TLS mismatch.
        let TransportError::Retryable(message) = request_error(connection) else {
            panic!("a refused connection is retryable");
        };
        assert!(message.contains(": "), "{message}");
        assert!(message.to_lowercase().contains("connect"), "{message}");

        let invalid = reqwest::Client::new()
            .get("http://[invalid")
            .send()
            .await
            .unwrap_err();
        assert!(matches!(request_error(invalid), TransportError::Fatal(_)));
    }

    #[tokio::test]
    async fn send_failure_is_a_terminal_stream_error() {
        let events = stream(async { Err(retryable("connection failed")) }, TextMapper)
            .collect::<Vec<_>>()
            .await;

        assert!(
            matches!(events.as_slice(), [ProviderEvent::RetryableError(error)] if error == "connection failed")
        );
    }

    #[tokio::test]
    async fn pump_panic_is_a_terminal_stream_error() {
        let events = stream(
            async {
                panic!("transport boom");
                #[allow(unreachable_code)]
                Err(fatal("unreachable"))
            },
            TextMapper,
        )
        .collect::<Vec<_>>()
        .await;

        assert!(
            matches!(events.as_slice(), [ProviderEvent::Error(error)] if error == "internal error: transport boom")
        );
    }

    struct FailingMapper;

    impl EventMapper for FailingMapper {
        fn map(&mut self, _data: &str) -> Result<Vec<ProviderEvent>, String> {
            Err("unknown delta type".into())
        }

        fn finish(&mut self) -> Option<ProviderEvent> {
            None
        }
    }

    #[test]
    fn error_chains_include_sources() {
        #[derive(Debug, thiserror::Error)]
        #[error("error decoding response body")]
        struct Outer(#[source] std::io::Error);
        let error = Outer(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "operation timed out",
        ));
        let message = error_with_sources(&error);
        assert_eq!(message, "error decoding response body: operation timed out");
    }

    #[tokio::test]
    async fn decode_errors_carry_a_scrubbed_event_snippet() {
        let response = response("data: {\"delta\":\"weird\",\"token\":\"sk-secret\"}\n\n").await;
        let events = stream(
            async {
                Ok(TransportResponse {
                    response,
                    secrets: vec!["sk-secret".into()],
                    provider: "zai",
                    credential: Credential::ApiKey,
                })
            },
            FailingMapper,
        )
        .collect::<Vec<_>>()
        .await;

        let [ProviderEvent::Error(error)] = events.as_slice() else {
            panic!("expected a single terminal error: {events:?}");
        };
        assert!(error.contains("unknown delta type"), "{error}");
        assert!(
            error.contains("offending event") && error.contains("\"weird\""),
            "{error}"
        );
        assert!(!error.contains("sk-secret"), "{error}");
        assert!(error.contains("<redacted>"), "{error}");
    }

    #[test]
    fn decode_error_snippets_are_bounded() {
        let long = "x".repeat(MAX_EVENT_SNIPPET_CHARS * 4);
        let error = decode_error("boom".into(), &long, &[]);
        assert!(
            error.chars().count() < MAX_EVENT_SNIPPET_CHARS + 50,
            "{}",
            error.len()
        );
        assert!(error.ends_with('…'), "{error}");
    }

    #[tokio::test]
    async fn sse_pump_stops_at_the_first_terminal_event() {
        let response = response("data: hello\n\ndata: done\n\ndata: trailing\n\n").await;
        let events = stream(
            async {
                Ok(TransportResponse {
                    response,
                    secrets: Vec::new(),
                    provider: "zai",
                    credential: Credential::ApiKey,
                })
            },
            TerminalMapper,
        )
        .collect::<Vec<_>>()
        .await;

        assert!(matches!(events.first(), Some(ProviderEvent::TextDelta(text)) if text == "hello"));
        assert!(matches!(
            events.get(1),
            Some(ProviderEvent::TurnComplete { .. })
        ));
        assert_eq!(events.len(), 2);
    }

    struct DropSignal(Option<tokio::sync::oneshot::Sender<()>>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    #[tokio::test]
    async fn dropping_stream_aborts_the_in_flight_transport() {
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let mut events = stream(
            async move {
                let _guard = DropSignal(Some(dropped_tx));
                let _ = started_tx.send(());
                std::future::pending::<Result<TransportResponse, TransportError>>().await
            },
            TextMapper,
        );
        let poll = tokio::spawn(async move { events.next().await });
        tokio::time::timeout(std::time::Duration::from_secs(1), started_rx)
            .await
            .expect("transport task should start")
            .expect("transport start signal should be delivered");

        poll.abort();
        let _ = poll.await;
        tokio::time::timeout(std::time::Duration::from_secs(1), dropped_rx)
            .await
            .expect("dropping provider stream should abort its transport task")
            .expect("drop signal should be delivered");
    }
}
