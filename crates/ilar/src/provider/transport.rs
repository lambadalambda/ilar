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
pub(super) fn streaming_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(IDLE_TIMEOUT)
        .build()
        .expect("valid provider HTTP client")
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
    )
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
}

pub(super) enum TransportError {
    Retryable(String),
    Fatal(String),
}

pub(super) fn retryable(error: impl ToString) -> TransportError {
    TransportError::Retryable(error.to_string())
}

pub(super) fn request_error(error: reqwest::Error) -> TransportError {
    if error.is_connect() || error.is_timeout() || error.is_body() {
        retryable(error)
    } else {
        fatal(error)
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
        let TransportResponse { response, secrets } = match send.await {
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
            let secret_refs = secrets.iter().map(String::as_str).collect::<Vec<_>>();
            let body = super::error_body::bounded_error_body(response, &secret_refs).await;
            let event = if retryable_status(status) {
                ProviderEvent::RetryableError(format!("HTTP {status}: {body}"))
            } else {
                ProviderEvent::Error(format!("HTTP {status}: {body}"))
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
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let body = body.to_string();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            let _ = socket.read(&mut request).await.unwrap();
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{body}",
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
    async fn request_errors_retry_connections_but_not_invalid_requests() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let connection = reqwest::Client::new()
            .get(format!("http://{address}"))
            .send()
            .await
            .unwrap_err();
        assert!(matches!(
            request_error(connection),
            TransportError::Retryable(_)
        ));

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
