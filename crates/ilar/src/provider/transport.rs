use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::task::{Context, Poll};

use futures::{FutureExt, Stream, StreamExt};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use super::sse::SseParser;
use super::{EventStream, ProviderEvent};

pub(super) const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

pub(super) struct TransportResponse {
    pub response: reqwest::Response,
    pub secrets: Vec<String>,
}

pub(super) trait EventMapper: Send + 'static {
    fn map(&mut self, data: &str) -> Result<Vec<ProviderEvent>, String>;
    fn finish(&mut self) -> Option<ProviderEvent>;
}

pub(super) fn stream<F, M>(send: F, mut mapper: M) -> EventStream
where
    F: Future<Output = Result<TransportResponse, String>> + Send + 'static,
    M: EventMapper,
{
    let (tx, rx) = mpsc::channel(64);
    let tx_panic = tx.clone();
    let pump = async move {
        let TransportResponse { response, secrets } = match send.await {
            Ok(response) => response,
            Err(error) => {
                let _ = tx.send(ProviderEvent::Error(error)).await;
                return;
            }
        };
        if !response.status().is_success() {
            let status = response.status();
            let secret_refs = secrets.iter().map(String::as_str).collect::<Vec<_>>();
            let body = super::error_body::bounded_error_body(response, &secret_refs).await;
            let _ = tx
                .send(ProviderEvent::Error(format!("HTTP {status}: {body}")))
                .await;
            return;
        }

        let mut parser = SseParser::new();
        let mut bytes = response.bytes_stream();
        while let Some(chunk) = bytes.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    let _ = tx.send(ProviderEvent::Error(error.to_string())).await;
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
                        let _ = tx.send(ProviderEvent::Error(error)).await;
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

fn is_terminal(event: &ProviderEvent) -> bool {
    matches!(
        event,
        ProviderEvent::TurnComplete { .. } | ProviderEvent::Error(_)
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

    #[tokio::test]
    async fn send_failure_is_a_terminal_stream_error() {
        let events = stream(async { Err("connection failed".into()) }, TextMapper)
            .collect::<Vec<_>>()
            .await;

        assert!(
            matches!(events.as_slice(), [ProviderEvent::Error(error)] if error == "connection failed")
        );
    }

    #[tokio::test]
    async fn pump_panic_is_a_terminal_stream_error() {
        let events = stream(
            async {
                panic!("transport boom");
                #[allow(unreachable_code)]
                Err("unreachable".into())
            },
            TextMapper,
        )
        .collect::<Vec<_>>()
        .await;

        assert!(
            matches!(events.as_slice(), [ProviderEvent::Error(error)] if error == "internal error: transport boom")
        );
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
                std::future::pending::<Result<TransportResponse, String>>().await
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
