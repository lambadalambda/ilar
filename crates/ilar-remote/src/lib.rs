//! Remote control of one ilar session over iroh.
//!
//! A session starts a [`host::RemoteHost`] with a fresh key; its
//! [`Ticket`] (endpoint id, relay, one-time secret) goes into a QR code.
//! The first peer that shows the secret is pinned, every other peer is
//! refused. See meta/issues/remote-control-over-iroh-index.md.

pub mod browser;
pub mod client;
#[cfg(not(target_family = "wasm"))]
pub mod host;
pub mod pairing;
pub mod ticket;
pub mod wire;

pub use iroh;
pub use iroh::{EndpointId, SecretKey};
pub use pairing::{Pairing, Secret, Verdict};
pub use ticket::Ticket;

/// The ALPN of the remote protocol; bump the version on a wire change.
pub const ALPN: &[u8] = b"ilar/remote/1";

/// Where the PWA lives; the ticket rides in the link's `#fragment`, so
/// it never reaches the web server.
pub const DEFAULT_WEB_BASE: &str = "https://lambadalambda.github.io/ilar/";

/// Close code a refused peer sees: final, scan a new code.
pub const REFUSED: u32 = 403;
/// Close code for a handshake the host could not take now (too many
/// at once, or a hello that never came): try again.
pub const BUSY: u32 = 503;
