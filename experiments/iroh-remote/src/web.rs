//! Browser side of the spike, exported through wasm-bindgen.

use crate::{ALPN, MAX_MESSAGE, decode_ticket};
use iroh::{Endpoint, endpoint::Connection, endpoint::presets};
use wasm_bindgen::prelude::*;

fn js(e: impl std::fmt::Display) -> JsError {
    JsError::new(&e.to_string())
}

#[wasm_bindgen]
pub struct Client {
    _endpoint: Endpoint,
    conn: Connection,
}

#[wasm_bindgen]
impl Client {
    /// Binds a browser endpoint and dials the ticket's host.
    pub async fn connect(ticket: String) -> Result<Client, JsError> {
        console_error_panic_hook::set_once();
        let addr = decode_ticket(&ticket).map_err(|e| js(format!("{e:#}")))?;
        let endpoint = Endpoint::bind(presets::N0).await.map_err(js)?;
        let conn = endpoint.connect(addr, ALPN).await.map_err(js)?;
        Ok(Client {
            _endpoint: endpoint,
            conn,
        })
    }

    /// Sends one JSON request and resolves to the host's JSON reply.
    pub fn request(&self, json: String) -> js_sys::Promise {
        let conn = self.conn.clone();
        wasm_bindgen_futures::future_to_promise(async move {
            let (mut send, mut recv) = conn.open_bi().await.map_err(js)?;
            send.write_all(json.as_bytes()).await.map_err(js)?;
            send.finish().map_err(js)?;
            let reply = recv.read_to_end(MAX_MESSAGE).await.map_err(js)?;
            Ok(JsValue::from_str(&String::from_utf8_lossy(&reply)))
        })
    }
}
