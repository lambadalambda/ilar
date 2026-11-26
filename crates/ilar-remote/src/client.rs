//! The handshake, from the side that dials: one bi-stream with a JSON
//! `Hello`, answered by `{"ok":true}` or by the host closing the
//! connection with [`crate::REFUSED`]. Builds for wasm32 too.

use crate::pairing::Secret;
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use iroh::endpoint::Connection;
use serde::{Deserialize, Serialize};

pub(crate) const MAX_HELLO: usize = 1024;

#[derive(Serialize, Deserialize)]
pub(crate) struct Hello {
    /// base64url; absent when a pinned peer comes back.
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

/// Presents `secret` (or nothing, as a pinned peer) on `conn`.
pub async fn hello(conn: &Connection, secret: Option<&Secret>) -> Result<()> {
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(&serde_json::to_vec(&Hello::new(secret))?)
        .await?;
    send.finish()?;
    let reply = recv
        .read_to_end(MAX_HELLO)
        .await
        .context("the session refused this device")?;
    let welcome: Welcome = serde_json::from_slice(&reply).context("the session's reply")?;
    if !welcome.ok {
        bail!("the session refused this device");
    }
    Ok(())
}
