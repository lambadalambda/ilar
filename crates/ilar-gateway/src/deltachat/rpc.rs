//! JSON-RPC 2.0 over `deltachat-rpc-server`'s stdio: one object per
//! line each way, requests matched to responses by id. The server's
//! event stream is an ordinary request (`get_next_event`) that answers
//! when there is one, so a single reader serves both.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex as AsyncMutex, oneshot};

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;

pub struct Rpc {
    writer: AsyncMutex<Box<dyn AsyncWrite + Unpin + Send>>,
    pending: Pending,
    next_id: AtomicU64,
    reader: tokio::task::JoinHandle<()>,
    child: Mutex<Option<tokio::process::Child>>,
}

impl Rpc {
    /// Start the server with its accounts under `accounts_dir`, and
    /// take its stdio. The process dies with this value.
    pub fn spawn(program: &Path, accounts_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(accounts_dir)
            .with_context(|| format!("creating {}", accounts_dir.display()))?;
        let mut command = tokio::process::Command::new(program);
        command
            .env("DC_ACCOUNTS_PATH", accounts_dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true);
        // The server logs every IMAP round trip at info; warnings are
        // what a gateway log wants to see.
        if std::env::var_os("RUST_LOG").is_none() {
            command.env("RUST_LOG", "warn");
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("starting {}", program.display()))?;
        let stdin = child.stdin.take().context("rpc server has no stdin")?;
        let stdout = child.stdout.take().context("rpc server has no stdout")?;
        let rpc = Self::over(stdout, stdin);
        *rpc.child.lock().unwrap() = Some(child);
        Ok(rpc)
    }

    /// A client over any pair of streams — what a test hands a fake
    /// server through `tokio::io::duplex`.
    pub fn over(
        reader: impl AsyncRead + Unpin + Send + 'static,
        writer: impl AsyncWrite + Unpin + Send + 'static,
    ) -> Self {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let reader_pending = pending.clone();
        let reader = tokio::spawn(async move {
            let mut lines = BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let Some(id) = message.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                let Some(waiter) = reader_pending.lock().unwrap().remove(&id) else {
                    continue;
                };
                let outcome = match message.get("error") {
                    Some(error) => Err(anyhow!(
                        "{}",
                        error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("rpc error")
                    )),
                    None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = waiter.send(outcome);
            }
            // The stream is gone: nobody waiting will ever be answered.
            let waiters: Vec<_> = reader_pending.lock().unwrap().drain().collect();
            for (_, waiter) in waiters {
                let _ = waiter.send(Err(anyhow!("rpc server closed its output")));
            }
        });
        Self {
            writer: AsyncMutex::new(Box::new(writer)),
            pending,
            next_id: AtomicU64::new(1),
            reader,
            child: Mutex::new(None),
        }
    }

    /// One request, its result. `params` is the positional array the
    /// server expects.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let request = json!({"jsonrpc": "2.0", "method": method, "params": params, "id": id});
        let mut line = serde_json::to_string(&request)?;
        line.push('\n');
        {
            let mut writer = self.writer.lock().await;
            if let Err(error) = writer.write_all(line.as_bytes()).await {
                self.pending.lock().unwrap().remove(&id);
                return Err(error).with_context(|| format!("sending {method}"));
            }
            writer.flush().await?;
        }
        rx.await
            .unwrap_or_else(|_| Err(anyhow!("rpc server closed")))
            .with_context(|| format!("{method} failed"))
    }
}

impl Drop for Rpc {
    fn drop(&mut self) {
        self.reader.abort();
    }
}
