//! The handshake and the wire between two local endpoints, with no
//! relay and no network beyond this machine.

use ilar_remote::client::{self, Refused, Session};
use ilar_remote::host::{HostEvent, Link, RemoteHost};
use ilar_remote::wire::{FromDevice, ToDevice};
use ilar_remote::{ALPN, EndpointId, Secret};
use iroh::endpoint::{Builder, Connection, presets};
use iroh::{Endpoint, RelayMode};
use std::time::Duration;
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

async fn paired(host: &RemoteHost) -> (Endpoint, Connection, Session) {
    let (device, conn) = dial(host).await;
    let session = client::hello(&conn, Some(&host.ticket().secret))
        .await
        .unwrap();
    (device, conn, session)
}

/// The next event, which must be a link for `peer`.
async fn link_for(events: &mut UnboundedReceiver<HostEvent>, peer: EndpointId) -> Link {
    match events.recv().await {
        Some(HostEvent::Paired { peer: p, link } | HostEvent::Welcome { peer: p, link })
            if p == peer =>
        {
            link
        }
        other => panic!("expected a link for {peer}, got {other:?}"),
    }
}

fn is_refused(error: &anyhow::Error) -> bool {
    error.downcast_ref::<Refused>().is_some()
}

#[tokio::test]
async fn the_ticket_secret_pairs_a_device() {
    let (host, mut events) = host().await;
    let (device, _conn, _session) = paired(&host).await;
    assert!(matches!(
        events.recv().await,
        Some(HostEvent::Paired { peer, .. }) if peer == device.id()
    ));
}

#[tokio::test]
async fn a_wrong_secret_is_refused_as_such() {
    let (host, mut events) = host().await;
    let (device, conn) = dial(&host).await;
    let error = client::hello(&conn, Some(&Secret::generate()))
        .await
        .err()
        .expect("refused");
    assert!(is_refused(&error), "{error:#}");
    assert!(matches!(
        events.recv().await,
        Some(HostEvent::Refused(peer)) if peer == device.id()
    ));
}

#[tokio::test]
async fn devices_pair_while_the_window_is_open_and_each_keeps_its_link() {
    let (host, mut events) = host().await;
    let secret = host.ticket().secret;
    let (phone, _first, mut phone_session) = paired(&host).await;
    let phone_link = link_for(&mut events, phone.id()).await;
    let (desktop, _second, _desktop_session) = paired(&host).await;
    let _desktop_link = link_for(&mut events, desktop.id()).await;

    // The desktop's arrival did not replace the phone's connection.
    phone_link
        .to_device
        .send(ToDevice::Busy { busy: true })
        .unwrap();
    assert_eq!(
        phone_session.next().await.unwrap(),
        Some(ToDevice::Busy { busy: true })
    );

    host.set_open(false);
    let (_late, third) = dial(&host).await;
    let error = client::hello(&third, Some(&secret)).await.err().unwrap();
    assert!(is_refused(&error), "{error:#}");
}

#[tokio::test]
async fn messages_cross_the_link_both_ways() {
    let (host, mut events) = host().await;
    let (device, _conn, mut session) = paired(&host).await;
    let mut link = link_for(&mut events, device.id()).await;

    link.to_device.send(ToDevice::Busy { busy: true }).unwrap();
    assert_eq!(
        session.next().await.unwrap(),
        Some(ToDevice::Busy { busy: true })
    );
    session.send(&FromDevice::Cancel).await.unwrap();
    assert_eq!(link.from_device.recv().await, Some(FromDevice::Cancel));
}

#[tokio::test]
async fn the_paired_device_comes_back_and_the_newest_connection_wins() {
    let (host, mut events) = host().await;
    let (phone, _old_conn, mut old) = paired(&host).await;
    let first = link_for(&mut events, phone.id()).await;

    let again = phone.connect(host.endpoint().addr(), ALPN).await.unwrap();
    let _new = client::hello(&again, None).await.unwrap();
    let second = link_for(&mut events, phone.id()).await;
    assert_ne!(first.serial, second.serial);
    assert!(matches!(
        events.recv().await,
        Some(HostEvent::Disconnected { serial, .. }) if serial == first.serial
    ));
    let ended = tokio::time::timeout(Duration::from_secs(2), old.next()).await;
    assert!(matches!(ended, Ok(Ok(None)) | Ok(Err(_))), "{ended:?}");
}

#[tokio::test]
async fn a_dropped_host_admits_nobody() {
    let (host, _events) = host().await;
    let (addr, secret) = (host.endpoint().addr(), host.ticket().secret);
    drop(host);
    let device = local().bind().await.unwrap();
    let attempt = tokio::time::timeout(Duration::from_secs(2), async {
        let conn = device.connect(addr, ALPN).await?;
        client::hello(&conn, Some(&secret)).await
    })
    .await;
    assert!(!matches!(attempt, Ok(Ok(_))), "a dropped host paired");
}

#[tokio::test]
async fn each_start_has_its_own_key() {
    let (a, _) = host().await;
    let (b, _) = host().await;
    assert_ne!(a.ticket().id, b.ticket().id);
    assert_ne!(a.ticket().secret, b.ticket().secret);
}
