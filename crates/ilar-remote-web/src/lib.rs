//! The phone's side of `/remote`, for `web/remote/remote.js`: dial a
//! session from its link, then read and send wire messages as JSON
//! strings. The pairing pins this device's key, so the page keeps the
//! key ([`Remote::key`]) and passes it back on every reconnect; a new
//! key after a reload would be refused for good.
#![cfg(target_family = "wasm")]

use futures::lock::Mutex;
use ilar_remote::client::{self, Receiver, Refused, Sender};
use ilar_remote::iroh::endpoint::{Connection, presets};
use ilar_remote::iroh::{Endpoint, RelayMode, SecretKey};
use ilar_remote::wire::FromDevice;
use ilar_remote::{ALPN, Ticket, browser};
use std::rc::Rc;
use wasm_bindgen::prelude::*;

/// Errors carry a kind before a colon, so the page can tell a final
/// refusal (scan a new code) from a retry: `refused:`, `busy:`,
/// `ticket:`, or `network:`.
fn failure(kind: &str, error: impl std::fmt::Display) -> JsError {
    JsError::new(&format!("{kind}: {error}"))
}

#[wasm_bindgen]
pub struct Remote {
    endpoint: Endpoint,
    conn: Connection,
    sender: Rc<Mutex<Sender>>,
    receiver: Rc<Mutex<Receiver>>,
    key: [u8; 32],
}

/// Dials the session in `link` (a whole link, or the ticket alone) as
/// the device with `key` (32 bytes; a new key when empty). The ticket's
/// secret is shown every time: a pinned device is let in on its key
/// before the spent secret is even looked at.
#[wasm_bindgen]
pub async fn connect(link: String, key: Vec<u8>) -> Result<Remote, JsError> {
    console_error_panic_hook::set_once();
    let ticket = Ticket::decode(&link).map_err(|e| failure("ticket", format!("{e:#}")))?;
    let secret_key = match <[u8; 32]>::try_from(key.as_slice()) {
        Ok(bytes) => SecretKey::from_bytes(&bytes),
        Err(_) => SecretKey::generate(),
    };
    let key = secret_key.to_bytes();
    let ticket = Ticket {
        relay: ticket.relay.as_ref().map(browser::relay_url),
        ..ticket
    };
    let endpoint = Endpoint::builder(presets::N0)
        .relay_mode(RelayMode::Custom(browser::relay_map()))
        .secret_key(secret_key)
        .bind()
        .await
        .map_err(|e| failure("network", e))?;
    let conn = endpoint
        .connect(ticket.addr(), ALPN)
        .await
        .map_err(|e| failure("network", e))?;
    let session = client::hello(&conn, Some(&ticket.secret))
        .await
        .map_err(|error| {
            if error.downcast_ref::<Refused>().is_some() {
                failure("refused", error)
            } else if format!("{error}").contains("busy") {
                failure("busy", error)
            } else {
                failure("network", format!("{error:#}"))
            }
        })?;
    let (sender, receiver) = session.split();
    Ok(Remote {
        endpoint,
        conn,
        sender: Rc::new(Mutex::new(sender)),
        receiver: Rc::new(Mutex::new(receiver)),
        key,
    })
}

#[wasm_bindgen]
impl Remote {
    /// This device's key, to keep for the next connect.
    pub fn key(&self) -> Vec<u8> {
        self.key.to_vec()
    }

    /// This device's endpoint id: what the TUI shows as "device …".
    pub fn id(&self) -> String {
        self.endpoint.id().to_string()
    }

    /// The next message from the session as a JSON string, or `null`
    /// when the session closed the stream.
    pub fn next(&self) -> js_sys::Promise {
        let receiver = self.receiver.clone();
        wasm_bindgen_futures::future_to_promise(async move {
            let message = receiver
                .lock()
                .await
                .next()
                .await
                .map_err(|e| JsValue::from(failure("network", format!("{e:#}"))))?;
            Ok(match message {
                Some(message) => JsValue::from_str(
                    &serde_json::to_string(&message).expect("wire messages serialize"),
                ),
                None => JsValue::NULL,
            })
        })
    }

    /// Sends one command, given as the JSON of a `FromDevice`.
    pub fn send(&self, json: String) -> js_sys::Promise {
        let sender = self.sender.clone();
        wasm_bindgen_futures::future_to_promise(async move {
            let command: FromDevice =
                serde_json::from_str(&json).map_err(|e| JsValue::from(failure("command", e)))?;
            sender
                .lock()
                .await
                .send(&command)
                .await
                .map_err(|e| JsValue::from(failure("network", format!("{e:#}"))))?;
            Ok(JsValue::UNDEFINED)
        })
    }

    pub fn close(&self) {
        self.conn.close(0u32.into(), b"bye");
    }
}
