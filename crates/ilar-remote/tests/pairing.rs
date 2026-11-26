//! The handshake between two local endpoints, with no relay and no
//! network beyond this machine.

use ilar_remote::host::{HostEvent, RemoteHost};
use ilar_remote::{ALPN, Secret, client};
use iroh::endpoint::{Builder, Connection, presets};
use iroh::{Endpoint, RelayMode};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

fn local() -> Builder {
    Endpoint::builder(presets::Minimal).relay_mode(RelayMode::Disabled)
}

async fn host() -> (RemoteHost, UnboundedReceiver<HostEvent>) {
    let (tx, rx) = unbounded_channel();
    (RemoteHost::start(local(), tx).await.unwrap(), rx)
}

/// A device: its own endpoint, dialed in to `host`.
async fn dial(host: &RemoteHost) -> (Endpoint, Connection) {
    let device = local().bind().await.unwrap();
    let conn = device.connect(host.endpoint().addr(), ALPN).await.unwrap();
    (device, conn)
}

#[tokio::test]
async fn the_ticket_secret_pairs_a_device() {
    let (host, mut events) = host().await;
    let (device, conn) = dial(&host).await;
    client::hello(&conn, Some(&host.ticket().secret))
        .await
        .unwrap();
    assert_eq!(events.recv().await, Some(HostEvent::Paired(device.id())));
}

#[tokio::test]
async fn a_wrong_secret_is_refused() {
    let (host, mut events) = host().await;
    let (device, conn) = dial(&host).await;
    assert!(
        client::hello(&conn, Some(&Secret::generate()))
            .await
            .is_err()
    );
    assert_eq!(events.recv().await, Some(HostEvent::Refused(device.id())));
}

#[tokio::test]
async fn a_second_device_is_refused_after_pairing() {
    let (host, mut events) = host().await;
    let secret = host.ticket().secret;
    let (_phone, first) = dial(&host).await;
    client::hello(&first, Some(&secret)).await.unwrap();
    events.recv().await;
    let (other, second) = dial(&host).await;
    assert!(client::hello(&second, Some(&secret)).await.is_err());
    assert_eq!(events.recv().await, Some(HostEvent::Refused(other.id())));
}

#[tokio::test]
async fn the_paired_device_comes_back_without_the_secret() {
    let (host, mut events) = host().await;
    let (phone, conn) = dial(&host).await;
    client::hello(&conn, Some(&host.ticket().secret))
        .await
        .unwrap();
    conn.close(0u32.into(), b"bye");
    assert_eq!(events.recv().await, Some(HostEvent::Paired(phone.id())));
    assert_eq!(
        events.recv().await,
        Some(HostEvent::Disconnected(phone.id()))
    );

    let again = phone.connect(host.endpoint().addr(), ALPN).await.unwrap();
    client::hello(&again, None).await.unwrap();
    assert_eq!(events.recv().await, Some(HostEvent::Welcome(phone.id())));
}

#[tokio::test]
async fn a_dropped_host_admits_nobody() {
    let (host, _events) = host().await;
    let (addr, secret) = (host.endpoint().addr(), host.ticket().secret);
    drop(host);
    let device = local().bind().await.unwrap();
    let attempt = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let conn = device.connect(addr, ALPN).await?;
        client::hello(&conn, Some(&secret)).await
    })
    .await;
    assert!(!matches!(attempt, Ok(Ok(()))), "a dropped host paired");
}

#[tokio::test]
async fn each_start_has_its_own_key() {
    let (a, _) = host().await;
    let (b, _) = host().await;
    assert_ne!(a.ticket().id, b.ticket().id);
    assert_ne!(a.ticket().secret, b.ticket().secret);
}
