//! `ilar serve` — the HTTP view of the session store, and the write path
//! back into it.
//!
//! Four parts, in the order a request meets them: [`watch`] polls the
//! store and fans tails out, [`view`] projects events onto the wire,
//! [`drive`] runs turns for the write routes, and [`http`] is the router
//! over all three. This file is the process: resolve the token, bind,
//! say so once, serve.
//!
//! Reading still needs the state directory and nothing else — no
//! provider, no API key, no model, and none of them is validated at
//! startup, because a machine with no provider configured must still be
//! able to browse what it already recorded. The write path resolves its
//! runtime per turn, so a missing provider is an error on the one
//! request that needed it rather than a server that will not start.
#![allow(dead_code)]

pub(crate) mod drive;
pub(crate) mod http;
pub(crate) mod view;
pub(crate) mod watch;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};

use drive::Drive;
use http::{ServeState, TOKEN_ENV};
use ilar::config::Config;
use watch::{WatchConfig, Watcher};

/// The default bind: loopback, port 4527 — "ilar" on a phone keypad.
/// 7777 was the first choice and promptly collided with another app on
/// a real machine; a vanity port is less contested and easier to
/// remember.
pub(crate) const DEFAULT_BIND: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 4527);

#[derive(Debug, Clone)]
pub(crate) struct ServeOptions {
    /// `None` is the default bind, which may fall back to an ephemeral
    /// port when taken; an explicit address never falls back.
    pub(crate) bind: Option<SocketAddr>,
    pub(crate) open: bool,
    /// Overrides the poll intervals, and `ILAR_SERVE_POLL_MS` with them.
    pub(crate) poll_ms: Option<u64>,
}

/// Bind the requested address, or the default with an ephemeral
/// fallback. Only the *default* degrades: whoever typed an address
/// meant that address.
async fn bind(requested: Option<SocketAddr>) -> Result<tokio::net::TcpListener> {
    match requested {
        Some(address) => tokio::net::TcpListener::bind(address)
            .await
            .with_context(|| {
                format!(
                    "binding {address} — pick another port, or --bind 127.0.0.1:0 for an ephemeral one"
                )
            }),
        None => match tokio::net::TcpListener::bind(DEFAULT_BIND).await {
            Ok(listener) => Ok(listener),
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                eprintln!("note: {DEFAULT_BIND} is taken; using an ephemeral port");
                tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                    .await
                    .context("binding an ephemeral loopback port")
            }
            Err(error) => Err(error).with_context(|| format!("binding {DEFAULT_BIND}")),
        },
    }
}

/// Serve the store this configuration names until the process is
/// interrupted. The configuration is here for the write path only:
/// nothing about a read consults it beyond the state directory.
pub(crate) async fn run(config: &Config, options: ServeOptions) -> Result<()> {
    let root = config.state_dir().join("sessions");
    let watcher = Watcher::new(root.clone(), WatchConfig::from_env(options.poll_ms));
    // Warm the head cache before the first request rather than after:
    // a cold listing head-parses every session in the store (P8).
    let warming = watcher.clone();
    tokio::task::spawn_blocking(move || warming.refresh())
        .await
        .context("scanning the session directory")?;
    watcher.spawn_poller();

    let listener = bind(options.bind).await?;
    let address = listener.local_addr().context("reading the bound address")?;
    let token = http::required_token(&address, std::env::var(TOKEN_ENV).ok());
    let url = http::url_for(&address, token.as_deref());

    println!("ilar serve · reading {}", root.display());
    if !address.ip().is_loopback() {
        // Said plainly, once, where it cannot be missed: this is a
        // bearer token over plain HTTP, and it now buys the holder a
        // turn on this machine, not only a read of one.
        eprintln!(
            "warning: {address} is not loopback. Traffic and the token are unencrypted, and the token can start turns here — put this behind a VPN or an SSH tunnel, never on the public internet."
        );
    }
    println!("{url}");
    if options.open {
        // The fragment carries the token; a failure to open a browser is
        // not a failure to serve.
        if let Err(error) = crate::links::open_in_browser(&url) {
            eprintln!("warning: could not open a browser: {error}");
        }
    }

    let state = ServeState {
        watcher,
        token: token.map(Into::into),
        drive: Arc::new(Drive::new(
            config.clone(),
            ilar::session::SessionStore::new(root),
        )),
    };
    axum::serve(listener, http::router(state))
        .await
        .context("serving")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default bind degrades to an ephemeral port; whoever typed an
    /// address meant it, and gets the error with the escape hatch named.
    #[tokio::test]
    async fn the_default_bind_falls_back_but_an_explicit_one_fails_loudly() {
        // Occupy the default port when free; when another process holds
        // it (the situation that motivated the fallback), that serves
        // the same purpose.
        let _squatter = tokio::net::TcpListener::bind(DEFAULT_BIND).await;
        let fallback = bind(None).await.expect("default bind falls back");
        assert!(fallback.local_addr().unwrap().ip().is_loopback());

        let taken = fallback.local_addr().unwrap();
        let error = bind(Some(taken)).await.expect_err("explicit bind fails");
        let message = format!("{error:#}");
        assert!(message.contains("--bind 127.0.0.1:0"), "{message}");
        assert!(message.contains(&taken.to_string()), "{message}");
    }
}
