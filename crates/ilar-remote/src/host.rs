//! The session side: one endpoint per session, a fresh key each start,
//! so an old QR code, or one from another session, reaches nothing.

use crate::client::{Hello, MAX_HELLO, Welcome};
use crate::{ALPN, Pairing, REFUSED, Secret, Ticket, Verdict};
use anyhow::{Context, Result};
use iroh::endpoint::{Builder, Connection, Incoming, presets};
use iroh::{Endpoint, EndpointId};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

/// How long a dialing peer has to say hello.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to wait for the relay; without one a browser cannot dial,
/// so the start fails.
const ONLINE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostEvent {
    Paired(EndpointId),
    /// The pinned peer reconnected.
    Welcome(EndpointId),
    Refused(EndpointId),
    Disconnected(EndpointId),
}

/// Dropping the host stops it too: the accept task, and with it every
/// connection task, is aborted, so an old QR code reaches nothing.
pub struct RemoteHost {
    endpoint: Endpoint,
    ticket: Ticket,
    accept: JoinHandle<()>,
}

impl Drop for RemoteHost {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

impl RemoteHost {
    /// Binds through n0's public relay and waits (bounded) for it, so
    /// the ticket carries the relay a browser must use.
    pub async fn start_public(events: UnboundedSender<HostEvent>) -> Result<Self> {
        let host = Self::start(Endpoint::builder(presets::N0), events).await?;
        if tokio::time::timeout(ONLINE_TIMEOUT, host.endpoint.online())
            .await
            .is_err()
        {
            host.stop().await;
            anyhow::bail!("no iroh relay answered within {ONLINE_TIMEOUT:?}");
        }
        Ok(host.with_current_relay())
    }

    /// Binds `builder` with a fresh key and starts accepting.
    pub async fn start(builder: Builder, events: UnboundedSender<HostEvent>) -> Result<Self> {
        let endpoint = builder
            .secret_key(iroh::SecretKey::generate())
            .alpns(vec![ALPN.to_vec()])
            .bind()
            .await
            .context("binding the remote endpoint")?;
        let secret = Secret::generate();
        let ticket = Ticket {
            id: endpoint.id(),
            relay: None,
            secret,
        };
        let pairing = Arc::new(Mutex::new(Pairing::new(secret)));
        let accept = tokio::spawn(accept_loop(endpoint.clone(), pairing, events));
        Ok(Self {
            endpoint,
            ticket,
            accept,
        })
    }

    fn with_current_relay(mut self) -> Self {
        self.ticket.relay = self.endpoint.addr().relay_urls().next().cloned();
        self
    }

    pub fn ticket(&self) -> &Ticket {
        &self.ticket
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Like dropping, but also waits for the endpoint to close cleanly.
    pub async fn stop(self) {
        self.accept.abort();
        self.endpoint.close().await;
    }
}

/// Owns the connection tasks: aborting this loop drops the set, and
/// that aborts them.
async fn accept_loop(
    endpoint: Endpoint,
    pairing: Arc<Mutex<Pairing>>,
    events: UnboundedSender<HostEvent>,
) {
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            incoming = endpoint.accept() => match incoming {
                Some(incoming) => {
                    connections.spawn(serve(incoming, pairing.clone(), events.clone()));
                }
                None => return,
            },
            Some(_) = connections.join_next() => {}
        }
    }
}

async fn serve(
    incoming: Incoming,
    pairing: Arc<Mutex<Pairing>>,
    events: UnboundedSender<HostEvent>,
) {
    let Ok(conn) = incoming.await else { return };
    let peer = conn.remote_id();
    let (verdict, send) = match tokio::time::timeout(HELLO_TIMEOUT, read_hello(&conn)).await {
        Ok(Ok((hello, send))) => {
            let verdict = pairing
                .lock()
                .expect("pairing lock")
                .admit(peer, hello.secret().as_ref());
            (verdict, Some(send))
        }
        _ => (Verdict::Refused, None),
    };
    // Reported as soon as decided: the pin holds even if the welcome
    // below never arrives, and the TUI must not keep saying "waiting".
    let event = match verdict {
        Verdict::Paired => HostEvent::Paired(peer),
        Verdict::Welcome => HostEvent::Welcome(peer),
        Verdict::Refused => {
            conn.close(REFUSED.into(), b"refused");
            let _ = events.send(HostEvent::Refused(peer));
            return;
        }
    };
    let _ = events.send(event);
    if let Some(send) = send
        && welcome(send).await.is_ok()
    {
        // The wire (meta/issues/the-remote-wire.md) runs here; until
        // then the connection is held open and its end reported.
        conn.closed().await;
    }
    let _ = events.send(HostEvent::Disconnected(peer));
}

async fn read_hello(conn: &Connection) -> Result<(Hello, iroh::endpoint::SendStream)> {
    let (send, mut recv) = conn.accept_bi().await?;
    let bytes = recv.read_to_end(MAX_HELLO).await?;
    Ok((serde_json::from_slice(&bytes)?, send))
}

async fn welcome(mut send: iroh::endpoint::SendStream) -> Result<()> {
    send.write_all(&serde_json::to_vec(&Welcome { ok: true })?)
        .await?;
    send.finish()?;
    Ok(())
}
