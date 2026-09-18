//! Provider abstraction — see meta/issues/provider-trait.md.

mod error_body;
mod event;
mod mapper;
mod mock;
mod request;
mod sse;
mod transport;

pub mod chat;
pub mod openai;
pub mod opencode;
pub mod zai;
pub use event::{ProviderEvent, StopReason};
pub use mock::MockProvider;
pub use request::{Request, ToolDefinition, resolve_model};

use std::pin::Pin;
use std::sync::Arc;

use futures::stream::Stream;

pub type EventStream = Pin<Box<dyn Stream<Item = ProviderEvent> + Send>>;

/// A chat-completion provider. Implementations translate [`Request`] to
/// their wire format and stream neutral [`ProviderEvent`]s back.
///
/// ## Error surface
///
/// `stream()` itself only fails on local, pre-flight problems (bad model
/// id, missing key). All network/HTTP/stream-decode failures — DNS, TLS,
/// 4xx/5xx, malformed SSE — arrive as [`ProviderEvent::Error`] or
/// [`ProviderEvent::RetryableError`] *on the stream*. Consumers must handle
/// errors mid-stream.
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

    /// Wire-protocol prefix accepted by this provider. Prefix-neutral test
    /// and adapter providers may leave this unspecified.
    fn provider_prefix(&self) -> Option<&'static str> {
        None
    }
}

/// Provider selected for one writer-owned turn.
pub enum ProviderHandle<'a> {
    Borrowed(&'a dyn Provider),
    Owned(Box<dyn Provider>),
}

impl ProviderHandle<'_> {
    pub fn as_provider(&self) -> &dyn Provider {
        match self {
            Self::Borrowed(provider) => *provider,
            Self::Owned(provider) => provider.as_ref(),
        }
    }
}

/// Resolves the concrete provider matching a persisted effective model.
pub trait ProviderResolver: Send + Sync {
    fn resolve_provider(&self, model: &str) -> anyhow::Result<ProviderHandle<'_>>;

    /// The model's whole window. `None` means the session never
    /// compacts, so an implementation without a better source answers
    /// the catalog's row rather than nothing — see the blanket impl.
    fn context_limit(&self, _model: &str) -> Option<u64> {
        None
    }

    /// Maximum request input. Defaults to the total context limit when a
    /// provider does not expose more precise model metadata.
    fn input_limit(&self, model: &str) -> Option<u64> {
        self.context_limit(model)
    }

    /// Budget compaction measures against, and the budget the context
    /// meter displays. Must stay at or below `input_limit`, since that
    /// is what the provider actually rejects on.
    fn compaction_limit(&self, model: &str) -> Option<u64> {
        self.input_limit(model)
    }
}

/// The limits a bare provider answers with: the catalog's, since it
/// has a row for every model a configuration can route — including
/// the discovered and custom ones, registered at load. The trait's
/// defaults answer `None`, and `None` is not "unknown" downstream, it
/// is "never compact": a surface that embedded a provider one type
/// parameter short of the configured resolver grew its session until
/// the provider rejected it. Only a model in no row at all stays
/// limitless, and such a model cannot be reached through configuration.
fn catalog_context_limit(model: &str) -> Option<u64> {
    crate::model::find(model).map(|row| row.context_limit)
}

fn catalog_input_limit(model: &str) -> Option<u64> {
    crate::model::find(model).map(|row| row.input_limit)
}

fn catalog_compaction_limit(model: &str) -> Option<u64> {
    crate::model::find(model).map(crate::model::compaction_limit)
}

impl<T: Provider> ProviderResolver for T {
    fn resolve_provider(&self, model: &str) -> anyhow::Result<ProviderHandle<'_>> {
        ensure_provider_matches(self, model)?;
        Ok(ProviderHandle::Borrowed(self))
    }

    fn context_limit(&self, model: &str) -> Option<u64> {
        catalog_context_limit(model)
    }

    fn input_limit(&self, model: &str) -> Option<u64> {
        catalog_input_limit(model)
    }

    fn compaction_limit(&self, model: &str) -> Option<u64> {
        catalog_compaction_limit(model)
    }
}

/// Adapter for callers that intentionally use one shared provider.
pub struct FixedProviderResolver {
    provider: Arc<dyn Provider>,
}

impl FixedProviderResolver {
    pub fn new(provider: Arc<dyn Provider>) -> Self {
        Self { provider }
    }
}

impl ProviderResolver for FixedProviderResolver {
    fn resolve_provider(&self, model: &str) -> anyhow::Result<ProviderHandle<'_>> {
        ensure_provider_matches(self.provider.as_ref(), model)?;
        Ok(ProviderHandle::Borrowed(self.provider.as_ref()))
    }

    fn context_limit(&self, model: &str) -> Option<u64> {
        catalog_context_limit(model)
    }

    fn input_limit(&self, model: &str) -> Option<u64> {
        catalog_input_limit(model)
    }

    fn compaction_limit(&self, model: &str) -> Option<u64> {
        catalog_compaction_limit(model)
    }
}

fn ensure_provider_matches(provider: &dyn Provider, model: &str) -> anyhow::Result<()> {
    let (requested, _) = resolve_model(model)?;
    if let Some(actual) = provider.provider_prefix()
        && actual != requested
    {
        anyhow::bail!("provider {actual:?} cannot serve model {model:?}");
    }
    Ok(())
}
