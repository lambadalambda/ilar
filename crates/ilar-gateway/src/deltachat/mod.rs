//! Delta Chat, through `deltachat-rpc-server`.
//!
//! picoclaw reaches Delta Chat through a Python bridge over WebSocket;
//! the bridge itself needs about twenty calls of the rpc server, which
//! ships as a binary. This adapter spawns that binary and makes the
//! calls itself.

pub mod rpc;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::bus::{Inbound, Outbound};
use crate::channel::{Channel, ChannelFuture};
use crate::driver::log;
use rpc::Rpc;

/// `[channels.deltachat]`.
#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DeltaChatConfig {
    /// The rpc server binary; `deltachat-rpc-server` on PATH when unset.
    pub rpc_server: Option<PathBuf>,
    /// Where the account lives; `<gateway dir>/deltachat` when unset.
    pub accounts_dir: Option<PathBuf>,
    /// A `DCACCOUNT:` QR for a fresh chatmail identity …
    pub setup_qr: Option<String>,
    /// … or an existing address and password.
    pub addr: Option<String>,
    pub password: Option<String>,
    pub display_name: Option<String>,
    /// Addresses allowed to talk. Empty refuses to start unless
    /// `allow_anyone` says so.
    #[serde(default)]
    pub allow_from: Vec<String>,
    /// Talk to anyone who writes. Off: a bot with tools is not
    /// something to leave open by accident.
    #[serde(default)]
    pub allow_anyone: bool,
    /// An emoji to react with on receipt, as a "seen".
    pub ack_reaction: Option<String>,
}

/// Delta Chat's id for the account's own contact.
const SELF_CONTACT: u64 = 1;

pub struct DeltaChat {
    config: DeltaChatConfig,
    accounts_dir: PathBuf,
    /// A pre-connected transport, for tests; the server is spawned
    /// otherwise.
    transport: std::sync::Mutex<Option<Rpc>>,
    /// The connection `run` is using, for `send`.
    live: std::sync::Mutex<Option<Arc<Rpc>>>,
    account: AtomicU32,
}

impl DeltaChat {
    pub fn new(config: DeltaChatConfig, gateway_dir: &std::path::Path) -> Arc<Self> {
        let accounts_dir = config
            .accounts_dir
            .clone()
            .unwrap_or_else(|| gateway_dir.join("deltachat"));
        Arc::new(Self {
            config,
            accounts_dir,
            transport: std::sync::Mutex::new(None),
            live: std::sync::Mutex::new(None),
            account: AtomicU32::new(0),
        })
    }

    /// The adapter over a transport that already exists.
    pub fn over(config: DeltaChatConfig, rpc: Rpc) -> Arc<Self> {
        let channel = Self::new(config, std::path::Path::new("/nonexistent"));
        *channel.transport.lock().unwrap() = Some(rpc);
        channel
    }

    fn connect(&self) -> Result<Arc<Rpc>> {
        let rpc = match self.transport.lock().unwrap().take() {
            Some(rpc) => rpc,
            None => {
                let program = self
                    .config
                    .rpc_server
                    .clone()
                    .unwrap_or_else(|| PathBuf::from("deltachat-rpc-server"));
                Rpc::spawn(&program, &self.accounts_dir)?
            }
        };
        let rpc = Arc::new(rpc);
        *self.live.lock().unwrap() = Some(rpc.clone());
        Ok(rpc)
    }

    /// The account, configured and running. A fresh accounts directory
    /// gets one account, set up from the QR or the address.
    async fn setup(&self, rpc: &Rpc) -> Result<u32> {
        let accounts = rpc.call("get_all_accounts", json!([])).await?;
        let account = match accounts.as_array().and_then(|list| list.first()) {
            Some(first) => first
                .get("id")
                .and_then(Value::as_u64)
                .context("account without id")?,
            None => rpc
                .call("add_account", json!([]))
                .await?
                .as_u64()
                .context("add_account returned no id")?,
        };
        let configured = rpc
            .call("is_configured", json!([account]))
            .await?
            .as_bool()
            .unwrap_or(false);
        if !configured {
            match (
                &self.config.setup_qr,
                &self.config.addr,
                &self.config.password,
            ) {
                (Some(qr), _, _) => {
                    rpc.call("set_config_from_qr", json!([account, qr])).await?;
                }
                (None, Some(addr), Some(password)) => {
                    rpc.call("set_config", json!([account, "addr", addr]))
                        .await?;
                    rpc.call("set_config", json!([account, "mail_pw", password]))
                        .await?;
                }
                _ => bail!(
                    "the Delta Chat account is not configured and [channels.deltachat] names neither setup_qr nor addr and password"
                ),
            }
            log("deltachat: configuring the account");
            rpc.call("configure", json!([account])).await?;
        }
        if let Some(name) = &self.config.display_name {
            rpc.call("set_config", json!([account, "displayname", name]))
                .await?;
        }
        // A bot: no read receipts of its own, no contact-request
        // ceremony expected of it.
        rpc.call("set_config", json!([account, "bot", "1"])).await?;
        rpc.call("start_io", json!([account])).await?;
        let address = rpc
            .call("get_config", json!([account, "configured_addr"]))
            .await
            .ok()
            .and_then(|value| value.as_str().map(str::to_string))
            .unwrap_or_else(|| "unknown".into());
        log(&format!("deltachat: {address} is listening"));
        // A chatmail address is not enough to start a chat with: a
        // contact is added from a secure-join invite. Say it every
        // start and keep it in a file, so a person can be pointed at
        // it without reading the log.
        match rpc
            .call("get_chat_securejoin_qr_code", json!([account, null]))
            .await
        {
            Ok(Value::String(invite)) => {
                log(&format!("deltachat: invite {invite}"));
                let path = self.invite_path();
                if let Err(error) = std::fs::write(&path, format!("{invite}\n")) {
                    log(&format!(
                        "deltachat: invite not written to {}: {error}",
                        path.display()
                    ));
                }
            }
            Ok(other) => log(&format!("deltachat: no invite ({other})")),
            Err(error) => log(&format!("deltachat: no invite: {error:#}")),
        }
        if self.config.allow_from.is_empty() {
            if !self.config.allow_anyone {
                bail!(
                    "[channels.deltachat] has no allow_from; list the addresses that may talk, or set allow_anyone = true"
                );
            }
            log("deltachat: allow_anyone — whoever writes gets an answer");
        }
        Ok(account as u32)
    }

    /// Where the invite link is kept, beside the account.
    pub fn invite_path(&self) -> PathBuf {
        self.accounts_dir.join("invite.txt")
    }

    fn allowed(&self, address: &str) -> bool {
        self.config.allow_from.is_empty()
            || self
                .config
                .allow_from
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(address))
    }

    /// One `IncomingMsg`: fetched, filtered, handed to the bus.
    async fn incoming(
        &self,
        rpc: &Rpc,
        account: u32,
        chat_id: u64,
        msg_id: u64,
        inbound: &mpsc::Sender<Inbound>,
    ) -> Result<()> {
        let message = rpc.call("get_message", json!([account, msg_id])).await?;
        let from = message.get("fromId").and_then(Value::as_u64).unwrap_or(0);
        let flagged = |key: &str| message.get(key).and_then(Value::as_bool).unwrap_or(false);
        if from == SELF_CONTACT || flagged("isInfo") || flagged("isBot") {
            return Ok(());
        }
        let address = message
            .pointer("/sender/address")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if !self.allowed(&address) {
            log(&format!(
                "deltachat: ignoring {address} (not in allow_from)"
            ));
            return Ok(());
        }
        let chat = rpc
            .call("get_basic_chat_info", json!([account, chat_id]))
            .await?;
        if chat
            .get("isContactRequest")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            rpc.call("accept_chat", json!([account, chat_id])).await?;
        }
        let is_group = chat.get("chatType").and_then(Value::as_str) != Some("Single");
        let text = message
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let media = message
            .get("file")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .into_iter()
            .collect::<Vec<_>>();
        if text.trim().is_empty() && media.is_empty() {
            return Ok(());
        }
        if let Some(emoji) = &self.config.ack_reaction {
            let _ = rpc
                .call("send_reaction", json!([account, msg_id, [emoji]]))
                .await;
        }
        let _ = rpc.call("markseen_msgs", json!([account, [msg_id]])).await;
        inbound
            .send(Inbound {
                channel: self.name().to_string(),
                chat_id: chat_id.to_string(),
                sender_id: address,
                text,
                media,
                is_group,
            })
            .await
            .context("gateway stopped taking messages")
    }
}

impl Channel for DeltaChat {
    fn name(&self) -> &str {
        "deltachat"
    }

    fn constraints(&self) -> &str {
        "plain text, no markdown rendering; keep one message under 4000 characters; files are attached by absolute path"
    }

    fn run<'a>(
        &'a self,
        inbound: mpsc::Sender<Inbound>,
        cancel: CancellationToken,
    ) -> ChannelFuture<'a, Result<()>> {
        Box::pin(async move {
            let rpc = self.connect()?;
            let account = self.setup(&rpc).await?;
            self.account.store(account, Ordering::Release);
            loop {
                let event = tokio::select! {
                    () = cancel.cancelled() => return Ok(()),
                    event = rpc.call("get_next_event", json!([])) => event?,
                };
                if event.get("contextId").and_then(Value::as_u64) != Some(u64::from(account)) {
                    continue;
                }
                let Some(payload) = event.get("event") else {
                    continue;
                };
                match payload.get("kind").and_then(Value::as_str) {
                    Some("IncomingMsg") => {
                        let chat_id = payload.get("chatId").and_then(Value::as_u64);
                        let msg_id = payload.get("msgId").and_then(Value::as_u64);
                        if let (Some(chat_id), Some(msg_id)) = (chat_id, msg_id)
                            && let Err(error) = self
                                .incoming(&rpc, account, chat_id, msg_id, &inbound)
                                .await
                        {
                            log(&format!("deltachat: message {msg_id} dropped: {error:#}"));
                        }
                    }
                    Some("Error") => log(&format!(
                        "deltachat: {}",
                        payload
                            .get("msg")
                            .and_then(Value::as_str)
                            .unwrap_or("error")
                    )),
                    _ => {}
                }
            }
        })
    }

    fn send<'a>(&'a self, message: Outbound) -> ChannelFuture<'a, Result<()>> {
        Box::pin(async move {
            let rpc = self
                .live
                .lock()
                .unwrap()
                .clone()
                .context("deltachat is not connected")?;
            let account = self.account.load(Ordering::Acquire);
            let chat_id: u64 = message
                .chat_id
                .parse()
                .with_context(|| format!("chat id {:?} is not a Delta Chat id", message.chat_id))?;
            let mut text = Some(message.text).filter(|text| !text.trim().is_empty());
            if message.media.is_empty() {
                if let Some(text) = text {
                    rpc.call("misc_send_text_message", json!([account, chat_id, text]))
                        .await?;
                }
                return Ok(());
            }
            // The first file carries the text; the rest go bare.
            for path in &message.media {
                let data = json!({
                    "file": path,
                    "viewtype": if is_image(path) { "Image" } else { "File" },
                    "text": text.take(),
                });
                rpc.call("send_msg", json!([account, chat_id, data]))
                    .await?;
            }
            Ok(())
        })
    }
}

fn is_image(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    type Calls = Arc<Mutex<Vec<(String, Value)>>>;

    /// The rpc server as a test sees it: answers every call the adapter
    /// makes, records them, and hands out events as the test pushes
    /// them. An event poll is answered on its own task, since the real
    /// server keeps serving other calls while one waits.
    fn fake_server(
        configured: bool,
        messages: Vec<Value>,
        chats: Vec<Value>,
        events: mpsc::Receiver<Value>,
    ) -> (Rpc, Calls) {
        let (client_side, server_side) = tokio::io::duplex(64 * 1024);
        let (server_read, server_write) = tokio::io::split(server_side);
        let (client_read, client_write) = tokio::io::split(client_side);
        let calls: Calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let writer = Arc::new(tokio::sync::Mutex::new(server_write));
        let events = Arc::new(tokio::sync::Mutex::new(events));
        tokio::spawn(async move {
            let mut lines = BufReader::new(server_read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let request: Value = serde_json::from_str(&line).unwrap();
                let method = request["method"].as_str().unwrap().to_string();
                let params = request["params"].clone();
                recorded
                    .lock()
                    .unwrap()
                    .push((method.clone(), params.clone()));
                let id = request["id"].clone();
                if method == "get_next_event" {
                    let writer = writer.clone();
                    let events = events.clone();
                    tokio::spawn(async move {
                        let Some(event) = events.lock().await.recv().await else {
                            return;
                        };
                        let result = json!({"contextId": 1, "event": event});
                        reply(&writer, id, result).await;
                    });
                    continue;
                }
                let result = match method.as_str() {
                    "get_all_accounts" => json!([]),
                    "add_account" => json!(1),
                    "is_configured" => json!(configured),
                    "get_config" => json!("bot@example.org"),
                    "get_message" => messages
                        .iter()
                        .find(|m| m["id"] == params[1])
                        .cloned()
                        .unwrap_or(Value::Null),
                    "get_basic_chat_info" => chats
                        .iter()
                        .find(|c| c["id"] == params[1])
                        .cloned()
                        .unwrap_or(Value::Null),
                    "misc_send_text_message" => json!(42),
                    "send_msg" => json!(43),
                    "get_chat_securejoin_qr_code" => json!("https://i.delta.chat/#TEST"),
                    _ => Value::Null,
                };
                reply(&writer, id, result).await;
            }
        });
        (Rpc::over(client_read, client_write), calls)
    }

    async fn reply(
        writer: &tokio::sync::Mutex<tokio::io::WriteHalf<tokio::io::DuplexStream>>,
        id: Value,
        result: Value,
    ) {
        let response = json!({"jsonrpc": "2.0", "id": id, "result": result});
        let mut line = serde_json::to_string(&response).unwrap();
        line.push('\n');
        let _ = writer.lock().await.write_all(line.as_bytes()).await;
    }

    fn message(id: u64, chat: u64, from: u64, address: &str, text: &str) -> Value {
        json!({
            "id": id, "chatId": chat, "fromId": from, "text": text,
            "isInfo": false, "isBot": false, "file": null,
            "sender": {"address": address, "displayName": "Someone"},
        })
    }

    fn incoming(chat: u64, msg: u64) -> Value {
        json!({"kind": "IncomingMsg", "chatId": chat, "msgId": msg})
    }

    fn methods(calls: &Calls) -> Vec<String> {
        calls
            .lock()
            .unwrap()
            .iter()
            .map(|(m, _)| m.clone())
            .collect()
    }

    #[tokio::test]
    async fn a_fresh_account_is_set_up_from_the_qr_and_messages_reach_the_bus() {
        let (events, events_rx) = mpsc::channel(8);
        let (rpc, calls) = fake_server(
            false,
            vec![
                message(10, 5, 7, "alice@example.org", "hello bot"),
                message(11, 5, 1, "bot@example.org", "my own echo"),
                message(12, 6, 8, "mallory@example.org", "let me in"),
                message(13, 9, 7, "alice@example.org", "in the group"),
            ],
            vec![
                json!({"id": 5, "chatType": "Single", "isContactRequest": true}),
                json!({"id": 6, "chatType": "Single", "isContactRequest": true}),
                json!({"id": 9, "chatType": "Group", "isContactRequest": false}),
            ],
            events_rx,
        );
        let channel = DeltaChat::over(
            DeltaChatConfig {
                setup_qr: Some("DCACCOUNT:https://nine.testrun.org/new".into()),
                display_name: Some("ilar".into()),
                allow_from: vec!["Alice@example.org".into()],
                ..DeltaChatConfig::default()
            },
            rpc,
        );
        let (inbound_tx, mut inbound) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let runner = channel.clone();
        let run_cancel = cancel.clone();
        let running = tokio::spawn(async move { runner.run(inbound_tx, run_cancel).await });

        for (chat, msg) in [(5, 10), (5, 11), (6, 12), (9, 13)] {
            events.send(incoming(chat, msg)).await.unwrap();
        }
        let first = tokio::time::timeout(std::time::Duration::from_secs(5), inbound.recv())
            .await
            .expect("a message")
            .expect("open");
        assert_eq!(first.chat_id, "5");
        assert_eq!(first.sender_id, "alice@example.org");
        assert_eq!(first.text, "hello bot");
        assert!(!first.is_group);
        let second = tokio::time::timeout(std::time::Duration::from_secs(5), inbound.recv())
            .await
            .expect("the group message")
            .expect("open");
        assert_eq!(second.chat_id, "9");
        assert!(second.is_group, "{second:?}");
        // Own echo and the stranger never arrived.
        assert!(inbound.try_recv().is_err());

        let seen = methods(&calls);
        let setup: Vec<&str> = seen.iter().map(String::as_str).take(8).collect();
        assert_eq!(
            setup,
            [
                "get_all_accounts",
                "add_account",
                "is_configured",
                "set_config_from_qr",
                "configure",
                "set_config",
                "set_config",
                "start_io",
            ],
            "{seen:?}"
        );
        assert!(seen.iter().any(|m| m == "accept_chat"), "{seen:?}");
        assert!(
            seen.iter().any(|m| m == "get_chat_securejoin_qr_code"),
            "{seen:?}"
        );
        assert!(seen.iter().any(|m| m == "markseen_msgs"), "{seen:?}");
        cancel.cancel();
        let _ = running.await;
    }

    #[tokio::test]
    async fn an_open_allow_list_refuses_to_start() {
        let (_events, events_rx) = mpsc::channel(1);
        let (rpc, _calls) = fake_server(true, vec![], vec![], events_rx);
        let channel = DeltaChat::over(DeltaChatConfig::default(), rpc);
        let (inbound_tx, _inbound) = mpsc::channel(1);
        let error = channel
            .run(inbound_tx, CancellationToken::new())
            .await
            .expect_err("an empty allow_from must not start");
        assert!(error.to_string().contains("allow_from"), "{error:#}");
    }

    #[tokio::test]
    async fn sending_uses_text_and_file_messages() {
        let (_events, events_rx) = mpsc::channel(1);
        let (rpc, calls) = fake_server(true, vec![], vec![], events_rx);
        let channel = DeltaChat::over(
            DeltaChatConfig {
                allow_anyone: true,
                ..DeltaChatConfig::default()
            },
            rpc,
        );
        let (inbound_tx, _inbound) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let runner = channel.clone();
        let run_cancel = cancel.clone();
        let running = tokio::spawn(async move { runner.run(inbound_tx, run_cancel).await });
        // Let setup finish: the first event poll is the sign.
        for _ in 0..50 {
            if methods(&calls).iter().any(|m| m == "get_next_event") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        channel
            .send(Outbound {
                channel: "deltachat".into(),
                chat_id: "5".into(),
                text: "plain".into(),
                media: vec![],
            })
            .await
            .unwrap();
        channel
            .send(Outbound {
                channel: "deltachat".into(),
                chat_id: "5".into(),
                text: "with a picture".into(),
                media: vec!["/tmp/a.png".into(), "/tmp/b.bin".into()],
            })
            .await
            .unwrap();
        let recorded = calls.lock().unwrap().clone();
        let sends: Vec<&(String, Value)> = recorded
            .iter()
            .filter(|(m, _)| m == "misc_send_text_message" || m == "send_msg")
            .collect();
        assert_eq!(sends.len(), 3, "{sends:?}");
        assert_eq!(sends[0].1, json!([1, 5, "plain"]));
        assert_eq!(sends[1].1[2]["viewtype"], "Image");
        assert_eq!(sends[1].1[2]["text"], "with a picture");
        assert_eq!(sends[2].1[2]["viewtype"], "File");
        assert_eq!(sends[2].1[2]["text"], Value::Null);
        cancel.cancel();
        let _ = running.await;
    }
}
