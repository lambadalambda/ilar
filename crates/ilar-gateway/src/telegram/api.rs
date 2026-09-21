//! The Bot API wire: JSON calls, multipart uploads, file downloads.
//!
//! Behind a trait so the adapter can be driven against a fake in tests
//! the way the Delta Chat one is driven against a fake rpc server. The
//! real one is a thin `reqwest` client; the only care in it is that
//! the bot token is part of every URL, and so of every error `reqwest`
//! formats — it is scrubbed before an error leaves this module.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::channel::ChannelFuture;

pub trait BotApi: Send + Sync {
    /// One method with JSON parameters; the `result` of a reply whose
    /// `ok` is true, or the reply's description as the error.
    fn call<'a>(&'a self, method: &'a str, params: Value) -> ChannelFuture<'a, Result<Value>>;

    /// A method that takes a file: `fields` as form fields, the file
    /// under `field`.
    fn upload<'a>(
        &'a self,
        method: &'a str,
        fields: Value,
        field: &'a str,
        path: &'a Path,
    ) -> ChannelFuture<'a, Result<Value>>;

    /// Fetch a file the API named (`getFile`'s `file_path`) to `to`.
    fn download<'a>(&'a self, file_path: &'a str, to: &'a Path) -> ChannelFuture<'a, Result<()>>;
}

/// How long a call may take. A long poll waits `POLL_SECS` on the
/// server before answering; this leaves a margin over that.
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(75);

pub struct Http {
    client: reqwest::Client,
    token: String,
    base: String,
    files: String,
}

impl Http {
    pub fn new(token: &str) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(CALL_TIMEOUT)
            .build()
            .context("building the Telegram client")?;
        Ok(Self {
            client,
            token: token.to_string(),
            base: format!("https://api.telegram.org/bot{token}/"),
            files: format!("https://api.telegram.org/file/bot{token}/"),
        })
    }

    /// The token appears in every URL, so it appears in every error
    /// `reqwest` formats. Nothing with the token in it leaves here.
    fn scrub(&self, error: impl std::fmt::Display) -> anyhow::Error {
        anyhow::anyhow!("{}", error.to_string().replace(&self.token, "<token>"))
    }

    async fn finish(&self, method: &str, response: reqwest::Response) -> Result<Value> {
        let status = response.status();
        let body: Value = response
            .json()
            .await
            .map_err(|error| self.scrub(error))
            .with_context(|| format!("{method}: reading the reply ({status})"))?;
        result_of(method, body)
    }
}

/// The `result` of an `ok` reply, or the description of a refused one.
fn result_of(method: &str, body: Value) -> Result<Value> {
    if body.get("ok").and_then(Value::as_bool) == Some(true) {
        return Ok(body.get("result").cloned().unwrap_or(Value::Null));
    }
    let description = body
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("no description");
    let code = body
        .get("error_code")
        .and_then(Value::as_i64)
        .map(|code| format!(" ({code})"))
        .unwrap_or_default();
    bail!("{method}: {description}{code}")
}

impl BotApi for Http {
    fn call<'a>(&'a self, method: &'a str, params: Value) -> ChannelFuture<'a, Result<Value>> {
        Box::pin(async move {
            let response = self
                .client
                .post(format!("{}{method}", self.base))
                .json(&params)
                .send()
                .await
                .map_err(|error| self.scrub(error))
                .with_context(|| format!("{method}: request failed"))?;
            self.finish(method, response).await
        })
    }

    fn upload<'a>(
        &'a self,
        method: &'a str,
        fields: Value,
        field: &'a str,
        path: &'a Path,
    ) -> ChannelFuture<'a, Result<Value>> {
        Box::pin(async move {
            let bytes = tokio::fs::read(path)
                .await
                .with_context(|| format!("reading {}", path.display()))?;
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("file")
                .to_string();
            let mut form = reqwest::multipart::Form::new().part(
                field.to_string(),
                reqwest::multipart::Part::bytes(bytes).file_name(name),
            );
            if let Some(object) = fields.as_object() {
                for (key, value) in object {
                    let text = match value {
                        Value::String(text) => text.clone(),
                        Value::Null => continue,
                        other => other.to_string(),
                    };
                    form = form.text(key.clone(), text);
                }
            }
            let response = self
                .client
                .post(format!("{}{method}", self.base))
                .multipart(form)
                .send()
                .await
                .map_err(|error| self.scrub(error))
                .with_context(|| format!("{method}: upload failed"))?;
            self.finish(method, response).await
        })
    }

    fn download<'a>(&'a self, file_path: &'a str, to: &'a Path) -> ChannelFuture<'a, Result<()>> {
        Box::pin(async move {
            let response = self
                .client
                .get(format!("{}{file_path}", self.files))
                .send()
                .await
                .map_err(|error| self.scrub(error))
                .context("fetching a file")?;
            if !response.status().is_success() {
                bail!("fetching {file_path}: {}", response.status());
            }
            let bytes = response
                .bytes()
                .await
                .map_err(|error| self.scrub(error))
                .context("reading a file")?;
            if let Some(parent) = to.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(to, bytes)
                .await
                .with_context(|| format!("writing {}", to.display()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_refused_reply_is_an_error_with_the_description() {
        let error = result_of(
            "sendMessage",
            json!({"ok": false, "error_code": 400, "description": "Bad Request: chat not found"}),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "sendMessage: Bad Request: chat not found (400)"
        );
        assert_eq!(
            result_of("getMe", json!({"ok": true, "result": {"id": 1}})).unwrap(),
            json!({"id": 1})
        );
    }

    /// The token is in every URL; an error that quoted the URL would
    /// put it in the log.
    #[test]
    fn errors_never_carry_the_token() {
        let http = Http::new("123456:ABC-secret").unwrap();
        let scrubbed = http.scrub("https://api.telegram.org/bot123456:ABC-secret/getMe timed out");
        assert!(!scrubbed.to_string().contains("ABC-secret"), "{scrubbed}");
        assert!(scrubbed.to_string().contains("<token>"), "{scrubbed}");
    }
}
