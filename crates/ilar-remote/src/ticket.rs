//! What the QR code carries, kept small so the code stays scannable.
//!
//! Layout, base64url without padding: version (1 byte), endpoint id
//! (32), secret (16), relay URL (UTF-8, rest; may be empty). Direct
//! addresses are left out: a browser cannot use them, and a native
//! client finds them through the relay.

use crate::pairing::{SECRET_LEN, Secret};
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use iroh::{EndpointAddr, EndpointId, RelayUrl};

const VERSION: u8 = 1;
const HEAD: usize = 1 + 32 + SECRET_LEN;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ticket {
    pub id: EndpointId,
    pub relay: Option<RelayUrl>,
    pub secret: Secret,
}

impl Ticket {
    pub fn encode(&self) -> String {
        let mut bytes = Vec::with_capacity(HEAD + 40);
        bytes.push(VERSION);
        bytes.extend_from_slice(self.id.as_bytes());
        bytes.extend_from_slice(&self.secret.0);
        if let Some(relay) = &self.relay {
            bytes.extend_from_slice(relay.as_str().as_bytes());
        }
        URL_SAFE_NO_PAD.encode(bytes)
    }

    /// Takes a bare ticket or a whole link with the ticket after `#`.
    pub fn decode(text: &str) -> Result<Self> {
        let text = text.trim();
        let text = text.rsplit_once('#').map_or(text, |(_, t)| t);
        let bytes = URL_SAFE_NO_PAD
            .decode(text)
            .context("the ticket is not base64url")?;
        if bytes.len() < HEAD {
            bail!("the ticket is too short");
        }
        if bytes[0] != VERSION {
            bail!("ticket version {} is not known here", bytes[0]);
        }
        let id = EndpointId::from_bytes(bytes[1..33].try_into().expect("32 bytes"))
            .context("the ticket's endpoint id is not a key")?;
        let secret = Secret(bytes[33..HEAD].try_into().expect("secret length"));
        let relay = match &bytes[HEAD..] {
            [] => None,
            url => Some(
                std::str::from_utf8(url)
                    .ok()
                    .and_then(|u| u.parse().ok())
                    .context("the ticket's relay is not a URL")?,
            ),
        };
        Ok(Self { id, relay, secret })
    }

    pub fn link(&self, web_base: &str) -> String {
        format!("{web_base}#{}", self.encode())
    }

    pub fn addr(&self) -> EndpointAddr {
        let addr = EndpointAddr::new(self.id);
        match &self.relay {
            Some(relay) => addr.with_relay_url(relay.clone()),
            None => addr,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket(relay: Option<&str>) -> Ticket {
        Ticket {
            id: iroh::SecretKey::generate().public(),
            relay: relay.map(|r| r.parse().unwrap()),
            secret: Secret::generate(),
        }
    }

    #[test]
    fn a_ticket_round_trips_with_and_without_a_relay() {
        for t in [
            ticket(Some("https://euc1-1.relay.n0.iroh.link./")),
            ticket(None),
        ] {
            assert_eq!(Ticket::decode(&t.encode()).unwrap(), t);
        }
    }

    #[test]
    fn a_link_decodes_to_its_ticket_and_stays_short() {
        let t = ticket(Some("https://euc1-1.relay.n0.iroh.link./"));
        let link = t.link(crate::DEFAULT_WEB_BASE);
        assert_eq!(Ticket::decode(&link).unwrap(), t);
        assert!(link.len() < 160, "{} chars: {link}", link.len());
    }

    #[test]
    fn a_damaged_ticket_is_refused() {
        let encoded = ticket(None).encode();
        assert!(Ticket::decode(&encoded[..20]).is_err());
        assert!(Ticket::decode("not base64!").is_err());
        let mut future = URL_SAFE_NO_PAD.decode(&encoded).unwrap();
        future[0] = 9;
        assert!(Ticket::decode(&URL_SAFE_NO_PAD.encode(future)).is_err());
    }
}
