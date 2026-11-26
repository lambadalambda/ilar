//! Spike: can a browser reach a native iroh endpoint? See
//! meta/issues/iroh-reaches-a-browser.md.
//!
//! Wire: one bi-stream per request. The client writes a JSON value and
//! finishes; the host answers `{"echo": <value>}` and finishes.

use anyhow::{Context, Result};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use iroh::EndpointAddr;

pub const ALPN: &[u8] = b"ilar/remote-spike/0";
pub const MAX_MESSAGE: usize = 64 * 1024;

pub fn encode_ticket(addr: &EndpointAddr) -> String {
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(addr).expect("EndpointAddr serializes"))
}

pub fn decode_ticket(ticket: &str) -> Result<EndpointAddr> {
    let bytes = URL_SAFE_NO_PAD
        .decode(ticket.trim().trim_start_matches('#'))
        .context("ticket is not base64url")?;
    serde_json::from_slice(&bytes).context("ticket is not an endpoint address")
}

pub fn echo_reply(request: &[u8]) -> Vec<u8> {
    let value: serde_json::Value = serde_json::from_slice(request)
        .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }));
    serde_json::to_vec(&serde_json::json!({ "echo": value })).expect("json serializes")
}

#[cfg(target_family = "wasm")]
mod web;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ticket_round_trips() {
        let key = iroh::SecretKey::generate();
        let addr = EndpointAddr::new(key.public())
            .with_relay_url("https://relay.example/".parse().unwrap());
        assert_eq!(decode_ticket(&encode_ticket(&addr)).unwrap(), addr);
    }

    #[test]
    fn the_host_wraps_what_it_got() {
        assert_eq!(echo_reply(br#"{"hi":1}"#), br#"{"echo":{"hi":1}}"#);
    }
}
