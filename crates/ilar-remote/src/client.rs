//! The side that dials. It opens one bi-stream, sends a `Hello` line,
//! and gets a `Welcome` line back, or the host closes the connection
//! with [`crate::REFUSED`]. The same stream then carries the session
//! (see [`crate::wire`]). This file builds for wasm32 too.

use crate::pairing::Secret;
use crate::wire::{self, FromDevice, MAX_TO_DEVICE, Reader, ToDevice};
use anyhow::Result;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use iroh::endpoint::{Connection, ConnectionError, SendStream};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub(crate) struct Hello {
    /// base64url; absent when a paired peer comes back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

impl std::fmt::Debug for Hello {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let secret = self.secret.as_ref().map(|_| "..");
        f.debug_struct("Hello").field("secret", &secret).finish()
    }
}

impl Hello {
    pub fn new(secret: Option<&Secret>) -> Self {
        Self {
            secret: secret.map(|s| URL_SAFE_NO_PAD.encode(s.0)),
        }
    }

    /// A malformed secret counts as none: the pairing then refuses.
    #[cfg(not(target_family = "wasm"))]
    pub fn secret(&self) -> Option<Secret> {
        let bytes = URL_SAFE_NO_PAD.decode(self.secret.as_ref()?).ok()?;
        Some(Secret(bytes.try_into().ok()?))
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Welcome {
    pub ok: bool,
}

/// The session refused this device: wrong or spent secret, or another
/// device is paired. Distinct from a network error, which may be retried.
#[derive(Debug)]
pub struct Refused;

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the session refused this device")
    }
}

impl std::error::Error for Refused {}

/// A paired session, from the device's side.
pub struct Session {
    sender: Sender,
    receiver: Receiver,
}

impl Session {
    pub async fn send(&mut self, message: &FromDevice) -> Result<()> {
        self.sender.send(message).await
    }

    /// `None` when the session closed the stream.
    pub async fn next(&mut self) -> Result<Option<ToDevice>> {
        self.receiver.next().await
    }

    /// The two halves, for a caller that reads and sends at once.
    pub fn split(self) -> (Sender, Receiver) {
        (self.sender, self.receiver)
    }
}

pub struct Sender {
    send: SendStream,
}

impl Sender {
    /// Not cancel-safe: a send dropped halfway leaves half a line.
    pub async fn send(&mut self, message: &FromDevice) -> Result<()> {
        wire::write(&mut self.send, message).await
    }
}

pub struct Receiver {
    reader: Reader,
}

impl Receiver {
    /// `None` when the session closed the stream. Cancel-safe.
    pub async fn next(&mut self) -> Result<Option<ToDevice>> {
        self.reader.next().await
    }
}

/// Presents `secret` (or nothing, as a paired peer) on `conn`.
pub async fn hello(conn: &Connection, secret: Option<&Secret>) -> Result<Session> {
    let refused_or = |error: anyhow::Error| match close_code(conn) {
        Some(code) if code == crate::REFUSED => anyhow::Error::new(Refused),
        Some(code) if code == crate::BUSY => anyhow::anyhow!("the session is busy; try again"),
        _ => error,
    };
    let (mut send, recv) = conn.open_bi().await.map_err(|e| refused_or(e.into()))?;
    wire::write(&mut send, &Hello::new(secret))
        .await
        .map_err(refused_or)?;
    let mut reader = Reader::new(recv, MAX_TO_DEVICE);
    match reader.next::<Welcome>().await.map_err(refused_or)? {
        Some(Welcome { ok: true }) => Ok(Session {
            sender: Sender { send },
            receiver: Receiver { reader },
        }),
        _ => Err(refused_or(anyhow::anyhow!(
            "the session closed the handshake"
        ))),
    }
}

/// The application close code, once the host has closed `conn`.
fn close_code(conn: &Connection) -> Option<u32> {
    match conn.close_reason()? {
        ConnectionError::ApplicationClosed(close) => {
            u32::try_from(close.error_code.into_inner()).ok()
        }
        _ => None,
    }
}
