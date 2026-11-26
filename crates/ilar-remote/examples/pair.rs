//! Pairs with a session from the command line, through n0's relay,
//! like the phone will: `cargo run -p ilar-remote --example pair -- <link>`.
//! Holds the connection until Ctrl-C, so the TUI shows "connected".

use ilar_remote::{ALPN, Ticket, client};
use iroh::{Endpoint, endpoint::presets};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let link = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: pair <link or ticket>"))?;
    let ticket = Ticket::decode(&link)?;
    let endpoint = Endpoint::bind(presets::N0).await?;
    let started = std::time::Instant::now();
    let conn = endpoint.connect(ticket.addr(), ALPN).await?;
    client::hello(&conn, Some(&ticket.secret)).await?;
    println!("paired as {} in {:?}", endpoint.id(), started.elapsed());
    tokio::signal::ctrl_c().await?;
    conn.close(0u32.into(), b"bye");
    endpoint.close().await;
    Ok(())
}
