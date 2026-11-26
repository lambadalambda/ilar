//! Host side of the spike: binds an endpoint, prints a link (and its QR
//! code) that carries the ticket, and echoes JSON requests.
//!
//! Usage: spike-host [WEB_BASE]   (default http://localhost:8080/)

use anyhow::Result;
use iroh::{Endpoint, endpoint::presets};
use iroh_remote_spike::{ALPN, MAX_MESSAGE, echo_reply, encode_ticket};
use std::time::Instant;

#[tokio::main]
async fn main() -> Result<()> {
    let web_base = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "http://localhost:8080/".into());
    let started = Instant::now();
    let endpoint = Endpoint::builder(presets::N0)
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await?;
    endpoint.online().await;
    eprintln!("online after {:?}", started.elapsed());

    let link = format!("{web_base}#{}", encode_ticket(&endpoint.addr()));
    let qr = qrcode::QrCode::new(link.as_bytes())?
        .render::<qrcode::render::unicode::Dense1x2>()
        .quiet_zone(true)
        .build();
    println!("{qr}\n{link}");

    while let Some(incoming) = endpoint.accept().await {
        tokio::spawn(async move {
            if let Err(e) = serve(incoming).await {
                eprintln!("connection ended: {e:#}");
            }
        });
    }
    Ok(())
}

async fn serve(incoming: iroh::endpoint::Incoming) -> Result<()> {
    let conn = incoming.await?;
    eprintln!("connected: {}", conn.remote_id());
    loop {
        let (mut send, mut recv) = conn.accept_bi().await?;
        let request = recv.read_to_end(MAX_MESSAGE).await?;
        eprintln!("got {}", String::from_utf8_lossy(&request));
        send.write_all(&echo_reply(&request)).await?;
        send.finish()?;
    }
}
