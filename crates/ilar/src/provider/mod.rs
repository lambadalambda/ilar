//! Provider abstraction — see meta/issues/provider-trait.md.

mod event;
mod mock;
mod request;
mod sse;

pub mod openai;
pub mod zai;
pub use event::{ProviderEvent, StopReason};
pub use mock::MockProvider;
pub use request::{Request, ToolDefinition, resolve_model};

use std::pin::Pin;

use futures::stream::Stream;

pub type EventStream = Pin<Box<dyn Stream<Item = ProviderEvent> + Send>>;

/// A chat-completion provider. Implementations translate [`Request`] to
/// their wire format and stream neutral [`ProviderEvent`]s back.
///
/// ## Error surface
///
/// `stream()` itself only fails on local, pre-flight problems (bad model
/// id, missing key). All network/HTTP/stream-decode failures — DNS, TLS,
/// 4xx/5xx, malformed SSE — arrive as [`ProviderEvent::Error`] *on the
/// stream*. Consumers must handle `Error` mid-stream.
///
/// ## Cancellation
///
/// Dropping the returned stream aborts the underlying request. The
/// blessed implementation pattern: `tokio::spawn` a task that pumps the
/// HTTP response into an mpsc channel, and return a wrapper whose `Drop`
/// aborts the `JoinHandle` (dropping a bare `ReceiverStream` alone is not
/// enough — a quiet connection can linger).
pub trait Provider: Send + Sync {
    fn stream(&self, req: Request) -> anyhow::Result<EventStream>;
}
