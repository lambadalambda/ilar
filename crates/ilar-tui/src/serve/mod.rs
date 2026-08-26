//! `ilar serve` — the read-only HTTP view of the session store.
//!
//! Three parts, in the order a request meets them: [`watch`] polls the
//! store and fans tails out, [`view`] projects events onto the wire, and
//! [`http`] is the GET-only router over both. This file is the process:
//! resolve the token, bind, say so once, serve.
//!
//! It needs the state directory and nothing else — no provider, no API
//! key, no model, no validation of any of them. Reading a log that
//! already exists does not require the means to write another one.
#![allow(dead_code)]

pub(crate) mod http;
pub(crate) mod view;
pub(crate) mod watch;

use std::net::SocketAddr;
use std::path::Path;

use anyhow::{Context, Result};

use http::{ServeState, TOKEN_ENV};
use watch::{WatchConfig, Watcher};

#[derive(Debug, Clone)]
pub(crate) struct ServeOptions {
    pub(crate) bind: SocketAddr,
    pub(crate) open: bool,
    /// Overrides the poll intervals, and `ILAR_SERVE_POLL_MS` with them.
    pub(crate) poll_ms: Option<u64>,
}

/// Serve the store under `state_dir` until the process is interrupted.
pub(crate) async fn run(state_dir: &Path, options: ServeOptions) -> Result<()> {
    let root = state_dir.join("sessions");
    let watcher = Watcher::new(root.clone(), WatchConfig::from_env(options.poll_ms));
    // Warm the head cache before the first request rather than after:
    // a cold listing head-parses every session in the store (P8).
    let warming = watcher.clone();
    tokio::task::spawn_blocking(move || warming.refresh())
        .await
        .context("scanning the session directory")?;
    watcher.spawn_poller();

    let token = http::required_token(&options.bind, std::env::var(TOKEN_ENV).ok());
    let listener = tokio::net::TcpListener::bind(options.bind)
        .await
        .with_context(|| format!("binding {}", options.bind))?;
    let address = listener.local_addr().context("reading the bound address")?;
    let url = http::url_for(&address, token.as_deref());

    println!("ilar serve · reading {}", root.display());
    if !address.ip().is_loopback() {
        // Said plainly, once, where it cannot be missed: this is a
        // bearer token over plain HTTP.
        eprintln!(
            "warning: {address} is not loopback. Traffic and the token are unencrypted — put this behind a VPN or an SSH tunnel, never on the public internet."
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
    };
    axum::serve(listener, http::router(state))
        .await
        .context("serving")
}
