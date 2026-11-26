//! The session side. Each start makes one endpoint with a fresh key,
//! so an old QR code, or one from another session, reaches nothing.

use crate::client::{Hello, Welcome};
use crate::wire::{self, FromDevice, MAX_FROM_DEVICE, Reader, ToDevice};
use crate::{ALPN, BUSY, Pairing, REFUSED, Secret, Ticket, Verdict};
use anyhow::{Context, Result};
use iroh::endpoint::{Builder, Connection, Incoming, RecvStream, SendStream, presets};
use iroh::{Endpoint, EndpointId};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;

/// How long a dialing peer has to say hello.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to wait for the relay. A browser cannot dial without one,
/// so the start fails if none answers.
const ONLINE_TIMEOUT: Duration = Duration::from_secs(10);
/// Handshakes in flight at once. Anyone who saw the QR code knows the
/// id, so a flood of dials must not crowd out the real phone for long.
const MAX_HANDSHAKES: usize = 8;
/// At most one `Refused` per this long, so a dial loop cannot flood
/// the TUI with notices.
const REFUSED_QUIET: Duration = Duration::from_secs(10);

/// The paired device's half of a session: what to send it, and what it
/// sent. `serial` names the connection: a `Disconnected` for an older
/// serial is about a link that was already replaced.
#[derive(Debug)]
pub struct Link {
    pub serial: u64,
    pub to_device: UnboundedSender<ToDevice>,
    pub from_device: UnboundedReceiver<FromDevice>,
}

#[derive(Debug)]
pub enum HostEvent {
    Paired {
        peer: EndpointId,
        link: Link,
    },
    /// The pinned peer reconnected. Its older connection is closed:
    /// the newest one wins.
    Welcome {
        peer: EndpointId,
        link: Link,
    },
    Refused(EndpointId),
    Disconnected {
        peer: EndpointId,
        serial: u64,
    },
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
        let shared = Arc::new(Shared {
            pairing: Mutex::new(Pairing::new(secret)),
            current: Mutex::new(None),
            last_refused: Mutex::new(None),
            handshakes: Arc::new(Semaphore::new(MAX_HANDSHAKES)),
            serial: AtomicU64::new(0),
            events,
        });
        let accept = tokio::spawn(accept_loop(endpoint.clone(), shared));
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

struct Shared {
    pairing: Mutex<Pairing>,
    /// The paired device's live connection, so a newer one can close it.
    current: Mutex<Option<Connection>>,
    last_refused: Mutex<Option<Instant>>,
    handshakes: Arc<Semaphore>,
    serial: AtomicU64,
    events: UnboundedSender<HostEvent>,
}

impl Shared {
    fn refuse(&self, conn: &Connection) {
        conn.close(REFUSED.into(), b"refused");
        let mut last = self.last_refused.lock().expect("refused lock");
        if last.is_none_or(|at| at.elapsed() >= REFUSED_QUIET) {
            *last = Some(Instant::now());
            let _ = self.events.send(HostEvent::Refused(conn.remote_id()));
        }
    }

    /// Makes `conn` the device's connection, closing the one it
    /// replaces, and hands the TUI its link. One critical section, so
    /// two reconnects racing cannot leave the TUI holding the loser's
    /// link: serials and events come out in takeover order.
    fn take_over(
        &self,
        conn: &Connection,
        peer: EndpointId,
        verdict: Verdict,
    ) -> (
        u64,
        UnboundedReceiver<ToDevice>,
        UnboundedSender<FromDevice>,
    ) {
        let mut current = self.current.lock().expect("current lock");
        if let Some(previous) = current.replace(conn.clone()) {
            previous.close(0u32.into(), b"replaced");
        }
        let serial = self.serial.fetch_add(1, Ordering::Relaxed);
        let (to_device, outgoing) = unbounded_channel();
        let (incoming, from_device) = unbounded_channel();
        let link = Link {
            serial,
            to_device,
            from_device,
        };
        // Reported as soon as decided: the pin holds even if the
        // welcome never arrives, and the TUI must not keep saying
        // "waiting". The send does not block, so the lock is safe here.
        let _ = self.events.send(match verdict {
            Verdict::Paired => HostEvent::Paired { peer, link },
            _ => HostEvent::Welcome { peer, link },
        });
        (serial, outgoing, incoming)
    }
}

/// Owns the connection tasks: aborting this loop drops the set, and
/// that aborts them.
async fn accept_loop(endpoint: Endpoint, shared: Arc<Shared>) {
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            incoming = endpoint.accept() => match incoming {
                Some(incoming) => {
                    connections.spawn(serve(incoming, shared.clone()));
                }
                None => return,
            },
            Some(_) = connections.join_next() => {}
        }
    }
}

async fn serve(incoming: Incoming, shared: Arc<Shared>) {
    let Ok(conn) = incoming.await else { return };
    let peer = conn.remote_id();
    // Busy and a slow or garbled hello are not refusals: the device may
    // try again, and the TUI is not told.
    let Ok(permit) = shared.handshakes.clone().try_acquire_owned() else {
        conn.close(BUSY.into(), b"busy");
        return;
    };
    let handshake = tokio::time::timeout(HELLO_TIMEOUT, read_hello(&conn)).await;
    drop(permit);
    let Ok(Ok((hello, send, reader))) = handshake else {
        conn.close(BUSY.into(), b"no hello");
        return;
    };
    let verdict = shared
        .pairing
        .lock()
        .expect("pairing lock")
        .admit(peer, hello.secret().as_ref());
    if verdict == Verdict::Refused {
        shared.refuse(&conn);
        return;
    }
    let (serial, outgoing, incoming) = shared.take_over(&conn, peer, verdict);
    tokio::select! {
        _ = pump_out(send, outgoing) => {}
        _ = pump_in(reader, incoming) => {}
        _ = conn.closed() => {}
    }
    conn.close(0u32.into(), b"done");
    let _ = shared.events.send(HostEvent::Disconnected { peer, serial });
}

async fn read_hello(conn: &Connection) -> Result<(Hello, SendStream, Reader)> {
    let (send, recv): (SendStream, RecvStream) = conn.accept_bi().await?;
    let mut reader = Reader::new(recv, MAX_FROM_DEVICE);
    let hello = reader.next::<Hello>().await?.context("no hello")?;
    Ok((hello, send, reader))
}

/// Welcome first, then whatever the session sends, until the session
/// drops its sender or the stream fails.
async fn pump_out(mut send: SendStream, mut outgoing: UnboundedReceiver<ToDevice>) -> Result<()> {
    wire::write(&mut send, &Welcome { ok: true }).await?;
    while let Some(message) = outgoing.recv().await {
        wire::write(&mut send, &message).await?;
    }
    send.finish()?;
    // Let the last lines land before the caller closes the connection,
    // which would drop whatever is still unacknowledged.
    let _ = tokio::time::timeout(Duration::from_secs(2), send.stopped()).await;
    Ok(())
}

/// The device's commands, until it closes the stream. A malformed line
/// ends the link: the device is ours, so it is a bug, not noise.
async fn pump_in(mut reader: Reader, incoming: UnboundedSender<FromDevice>) -> Result<()> {
    while let Some(message) = reader.next::<FromDevice>().await? {
        if incoming.send(message).is_err() {
            break;
        }
    }
    Ok(())
}
