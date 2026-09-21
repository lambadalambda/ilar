//! Telegram, through the Bot API.
//!
//! Long polling and nothing inbound: the gateway keeps its "no HTTP
//! server" stance, and a bot behind NAT works the same as one on a
//! public box. What Telegram has that Delta Chat does not — a command
//! menu, inline buttons, reactions — is used where the gateway already
//! has the shape for it: the menu is `commands::MENU`, a button is a
//! command the person could have typed.

pub mod api;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::bus::{Button, Inbound, Outbound};
use crate::channel::{Channel, ChannelFuture};
use crate::driver::log;
use api::BotApi;

/// `[channels.telegram]`.
#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TelegramConfig {
    /// The bot token from BotFather.
    pub token: Option<String>,
    /// Who may talk: numeric user ids, or usernames with or without
    /// the `@`. Empty refuses to start unless `allow_anyone` says so.
    #[serde(default)]
    pub allow_from: Vec<String>,
    /// Talk to anyone who writes. Off: a bot with tools is not
    /// something to leave open by accident.
    #[serde(default)]
    pub allow_anyone: bool,
    /// An emoji to react with on receipt, as a "seen". Telegram takes
    /// only the ones it lists; an unlisted one is refused and logged.
    pub ack_reaction: Option<String>,
    /// Where fetched attachments go; `<gateway dir>/telegram` when unset.
    pub media_dir: Option<PathBuf>,
}

/// How long one `getUpdates` waits on the server. The API allows 50.
const POLL_SECS: u64 = 30;
/// After a failed poll — the network is down, Telegram is not — before
/// the next one.
const POLL_RETRY: std::time::Duration = std::time::Duration::from_secs(5);
/// Telegram's cap on a message is 4096 characters and on a caption
/// 1024. Pieces are cut by the gateway's line-counting splitter at 40
/// lines of 100, which keeps every piece under 4000.
const PIECE_LINES: usize = 40;
const PIECE_LINE_CHARS: usize = 100;
const CAPTION_CHARS: usize = 1024;
/// A button's callback data may be at most 64 bytes.
const CALLBACK_BYTES: usize = 64;

pub struct Telegram {
    config: TelegramConfig,
    api: Arc<dyn BotApi>,
    media_dir: PathBuf,
    /// The bot's own username from `getMe`, for stripping mentions.
    username: std::sync::Mutex<Option<String>>,
    bot_id: AtomicI64,
}

impl Telegram {
    pub fn new(config: TelegramConfig, gateway_dir: &Path) -> Result<Arc<Self>> {
        let token = config
            .token
            .as_deref()
            .filter(|token| !token.trim().is_empty())
            .context("[channels.telegram] needs a token")?;
        let api = Arc::new(api::Http::new(token)?);
        Ok(Self::over(config, api, gateway_dir))
    }

    /// The adapter over a wire that already exists.
    pub fn over(config: TelegramConfig, api: Arc<dyn BotApi>, gateway_dir: &Path) -> Arc<Self> {
        let media_dir = config
            .media_dir
            .clone()
            .unwrap_or_else(|| gateway_dir.join("telegram"));
        Arc::new(Self {
            config,
            api,
            media_dir,
            username: std::sync::Mutex::new(None),
            bot_id: AtomicI64::new(0),
        })
    }

    fn allowed(&self, user: &Value) -> bool {
        if self.config.allow_from.is_empty() {
            return true;
        }
        let id = user
            .get("id")
            .and_then(Value::as_i64)
            .map(|id| id.to_string());
        let username = user.get("username").and_then(Value::as_str);
        self.config.allow_from.iter().any(|allowed| {
            let allowed = allowed.trim();
            id.as_deref() == Some(allowed)
                || username.is_some_and(|name| {
                    allowed
                        .strip_prefix('@')
                        .unwrap_or(allowed)
                        .eq_ignore_ascii_case(name)
                })
        })
    }

    /// The bot's identity and its menu, once per run.
    async fn setup(&self) -> Result<()> {
        let me = self.api.call("getMe", json!({})).await?;
        let username = me
            .get("username")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        self.bot_id.store(
            me.get("id").and_then(Value::as_i64).unwrap_or(0),
            Ordering::Release,
        );
        *self.username.lock().unwrap() = Some(username.clone());
        // The menu: what typing `/` offers. Best effort — a menu that
        // would not set is a bot without one, not a bot that is down.
        let commands: Vec<Value> = crate::commands::MENU
            .iter()
            .map(|(name, description)| json!({"command": name, "description": description}))
            .collect();
        if let Err(error) = self
            .api
            .call("setMyCommands", json!({"commands": commands}))
            .await
        {
            log(&format!(
                "telegram: the command menu was not set: {error:#}"
            ));
        }
        if self.config.allow_from.is_empty() {
            if !self.config.allow_anyone {
                bail!(
                    "[channels.telegram] has no allow_from; list the user ids or usernames that may talk, or set allow_anyone = true"
                );
            }
            log("telegram: allow_anyone — whoever writes gets an answer");
        }
        log(&format!("telegram: @{username} is listening"));
        Ok(())
    }

    /// One update from the poll: a message or a tapped button.
    async fn update(&self, update: &Value, inbound: &mpsc::Sender<Inbound>) -> Result<()> {
        if let Some(message) = update.get("message") {
            return self.incoming(message, inbound).await;
        }
        if let Some(query) = update.get("callback_query") {
            return self.tapped(query, inbound).await;
        }
        Ok(())
    }

    async fn incoming(&self, message: &Value, inbound: &mpsc::Sender<Inbound>) -> Result<()> {
        let Some(from) = message.get("from") else {
            return Ok(());
        };
        // Other bots, and this one's own messages echoed back in a
        // group, are not people.
        if from.get("is_bot").and_then(Value::as_bool).unwrap_or(false) {
            return Ok(());
        }
        let sender_id = from
            .get("id")
            .and_then(Value::as_i64)
            .map(|id| id.to_string())
            .unwrap_or_default();
        if !self.allowed(from) {
            log(&format!(
                "telegram: ignoring {} (not in allow_from)",
                describe_user(from)
            ));
            return Ok(());
        }
        let chat = message.get("chat").cloned().unwrap_or(Value::Null);
        let chat_id = chat
            .get("id")
            .and_then(Value::as_i64)
            .map(|id| id.to_string())
            .context("a message without a chat")?;
        let is_group = chat.get("type").and_then(Value::as_str) != Some("private");
        let message_id = message
            .get("message_id")
            .and_then(Value::as_i64)
            .map(|id| id.to_string());
        let raw = message
            .get("text")
            .or_else(|| message.get("caption"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let username = self.username.lock().unwrap().clone().unwrap_or_default();
        let text = strip_mention(raw, &username);
        let media = match attachment(message) {
            Some(file) => self.fetch(&file).await.map(|path| vec![path])?,
            None => Vec::new(),
        };
        if text.trim().is_empty() && media.is_empty() {
            return Ok(());
        }
        if let (Some(emoji), Some(id)) = (&self.config.ack_reaction, message_id.as_deref()) {
            let reaction = json!({
                "chat_id": chat_id,
                "message_id": id.parse::<i64>().unwrap_or(0),
                "reaction": [{"type": "emoji", "emoji": emoji}],
            });
            if let Err(error) = self.api.call("setMessageReaction", reaction).await {
                log(&format!("telegram: reaction not set: {error:#}"));
            }
        }
        inbound
            .send(Inbound {
                channel: self.name().to_string(),
                chat_id,
                sender_id,
                message_id,
                text,
                media,
                is_group,
            })
            .await
            .context("gateway stopped taking messages")
    }

    /// A tapped button: the command it carries, from the person who
    /// tapped it, through the same door a typed one comes in. The
    /// query is answered first so the phone stops its spinner, and the
    /// keyboard comes off the message so a second tap answers nothing
    /// the first did not.
    async fn tapped(&self, query: &Value, inbound: &mpsc::Sender<Inbound>) -> Result<()> {
        if let Some(id) = query.get("id").and_then(Value::as_str)
            && let Err(error) = self
                .api
                .call("answerCallbackQuery", json!({"callback_query_id": id}))
                .await
        {
            log(&format!("telegram: callback not answered: {error:#}"));
        }
        let Some(from) = query.get("from") else {
            return Ok(());
        };
        if !self.allowed(from) {
            log(&format!(
                "telegram: ignoring a tap from {} (not in allow_from)",
                describe_user(from)
            ));
            return Ok(());
        }
        let Some(command) = query.get("data").and_then(Value::as_str) else {
            return Ok(());
        };
        let message = query.get("message").cloned().unwrap_or(Value::Null);
        let chat = message.get("chat").cloned().unwrap_or(Value::Null);
        let Some(chat_id) = chat.get("id").and_then(Value::as_i64) else {
            return Ok(());
        };
        if let Some(message_id) = message.get("message_id").and_then(Value::as_i64) {
            let bare = json!({
                "chat_id": chat_id,
                "message_id": message_id,
                "reply_markup": {"inline_keyboard": []},
            });
            if let Err(error) = self.api.call("editMessageReplyMarkup", bare).await {
                log(&format!("telegram: keyboard not removed: {error:#}"));
            }
        }
        inbound
            .send(Inbound {
                channel: self.name().to_string(),
                chat_id: chat_id.to_string(),
                sender_id: from
                    .get("id")
                    .and_then(Value::as_i64)
                    .map(|id| id.to_string())
                    .unwrap_or_default(),
                // The ask's own message, not a new one: nothing to
                // take back.
                message_id: None,
                text: command.to_string(),
                media: Vec::new(),
                is_group: chat.get("type").and_then(Value::as_str) != Some("private"),
            })
            .await
            .context("gateway stopped taking messages")
    }

    /// Fetch an attachment into the media directory, named after the
    /// file's own id and the name Telegram gives it.
    async fn fetch(&self, file: &Attachment) -> Result<PathBuf> {
        let info = self
            .api
            .call("getFile", json!({"file_id": file.file_id}))
            .await?;
        let file_path = info
            .get("file_path")
            .and_then(Value::as_str)
            .context("getFile answered without a path")?;
        let remote_name = Path::new(file_path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file");
        let name = match &file.name {
            Some(name) => name.clone(),
            None => remote_name.to_string(),
        };
        let to = self
            .media_dir
            .join(format!("{}-{}", file.unique_id, safe_name(&name)));
        self.api.download(file_path, &to).await?;
        Ok(to)
    }

    fn chat_id(message: &Outbound) -> Result<i64> {
        message
            .chat_id
            .parse()
            .with_context(|| format!("chat id {:?} is not a Telegram id", message.chat_id))
    }
}

/// One file on a message, whichever kind Telegram filed it under.
struct Attachment {
    file_id: String,
    unique_id: String,
    name: Option<String>,
}

/// The file a message carries, if any: a photo's largest size, or the
/// document, voice note, audio, video, animation or video note.
fn attachment(message: &Value) -> Option<Attachment> {
    let of = |value: &Value, name: Option<String>| {
        Some(Attachment {
            file_id: value.get("file_id")?.as_str()?.to_string(),
            unique_id: value
                .get("file_unique_id")
                .and_then(Value::as_str)
                .unwrap_or("file")
                .to_string(),
            name,
        })
    };
    if let Some(sizes) = message.get("photo").and_then(Value::as_array)
        && let Some(largest) = sizes.last()
    {
        return of(largest, Some("photo.jpg".into()));
    }
    for kind in [
        "document",
        "voice",
        "audio",
        "video",
        "animation",
        "video_note",
    ] {
        if let Some(value) = message.get(kind) {
            let name = value
                .get("file_name")
                .and_then(Value::as_str)
                .map(str::to_string);
            return of(value, name);
        }
    }
    None
}

/// A file name Telegram or a person chose, kept to characters that
/// cannot leave the media directory.
fn safe_name(name: &str) -> String {
    let kept: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let kept = kept.trim_matches('.').to_string();
    if kept.is_empty() {
        "file".to_string()
    } else {
        kept
    }
}

/// `/new@ilar_bot` is `/new`; `@ilar_bot hello` is `hello`. Telegram
/// addresses a bot in a group both ways.
fn strip_mention(text: &str, username: &str) -> String {
    let text = text.trim();
    if username.is_empty() {
        return text.to_string();
    }
    let suffix = format!("@{username}");
    if text.starts_with('/') {
        let (head, tail) = match text.split_once(char::is_whitespace) {
            Some((head, tail)) => (head, format!(" {tail}")),
            None => (text, String::new()),
        };
        if let Some(command) = head
            .strip_suffix(suffix.as_str())
            .filter(|command| !command.is_empty())
        {
            return format!("{command}{tail}").trim().to_string();
        }
        return text.to_string();
    }
    if text.len() >= suffix.len() && text[..suffix.len()].eq_ignore_ascii_case(&suffix) {
        return text[suffix.len()..].trim_start().to_string();
    }
    text.to_string()
}

fn describe_user(user: &Value) -> String {
    let id = user.get("id").and_then(Value::as_i64).unwrap_or(0);
    match user.get("username").and_then(Value::as_str) {
        Some(name) => format!("{id} (@{name})"),
        None => id.to_string(),
    }
}

/// An inline keyboard, two buttons to a row. A command too long for
/// Telegram's callback data is left off and named in the log; the
/// text of the message names it anyway.
fn keyboard(buttons: &[Button]) -> Option<Value> {
    let kept: Vec<Value> = buttons
        .iter()
        .filter(|button| {
            let fits = button.command.len() <= CALLBACK_BYTES;
            if !fits {
                log(&format!(
                    "telegram: button {:?} left off: its command is over {CALLBACK_BYTES} bytes",
                    button.label
                ));
            }
            fits
        })
        .map(|button| json!({"text": button.label, "callback_data": button.command}))
        .collect();
    if kept.is_empty() {
        return None;
    }
    let rows: Vec<Value> = kept.chunks(2).map(|row| json!(row)).collect();
    Some(json!({"inline_keyboard": rows}))
}

fn is_image(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp")
    )
}

impl Channel for Telegram {
    fn name(&self) -> &str {
        "telegram"
    }

    fn constraints(&self) -> &str {
        "plain text, no markdown rendering — asterisks and underscores show as typed; a long \
         text is sent as several messages, each under 40 lines of 100 characters, split at line \
         breaks, so write it whole; files are attached by path, pictures are shown as photos, \
         and a text that fits one message rides as the first file's caption"
    }

    fn run<'a>(
        &'a self,
        inbound: mpsc::Sender<Inbound>,
        cancel: CancellationToken,
    ) -> ChannelFuture<'a, Result<()>> {
        Box::pin(async move {
            self.setup().await?;
            let mut offset: Option<i64> = None;
            loop {
                let params = json!({
                    "offset": offset,
                    "timeout": POLL_SECS,
                    "allowed_updates": ["message", "callback_query"],
                });
                let updates = tokio::select! {
                    () = cancel.cancelled() => return Ok(()),
                    updates = self.api.call("getUpdates", params) => updates,
                };
                let updates = match updates {
                    Ok(updates) => updates,
                    Err(error) => {
                        // The network, most likely: wait and ask again
                        // rather than restart the channel for it.
                        log(&format!("telegram: poll failed: {error:#}"));
                        tokio::select! {
                            () = cancel.cancelled() => return Ok(()),
                            () = tokio::time::sleep(POLL_RETRY) => {}
                        }
                        continue;
                    }
                };
                for update in updates.as_array().into_iter().flatten() {
                    if let Some(id) = update.get("update_id").and_then(Value::as_i64) {
                        offset = Some(offset.map_or(id + 1, |current| current.max(id + 1)));
                    }
                    if let Err(error) = self.update(update, &inbound).await {
                        log(&format!("telegram: update dropped: {error:#}"));
                    }
                }
            }
        })
    }

    fn post_status<'a>(
        &'a self,
        chat_id: &'a str,
        text: &'a str,
    ) -> ChannelFuture<'a, Result<Option<String>>> {
        Box::pin(async move {
            let chat_id: i64 = chat_id.parse()?;
            let sent = self
                .api
                .call("sendMessage", json!({"chat_id": chat_id, "text": text}))
                .await?;
            Ok(sent
                .get("message_id")
                .and_then(Value::as_i64)
                .map(|id| id.to_string()))
        })
    }

    fn edit_status<'a>(
        &'a self,
        chat_id: &'a str,
        status_id: &'a str,
        text: &'a str,
    ) -> ChannelFuture<'a, Result<()>> {
        Box::pin(async move {
            let chat_id: i64 = chat_id.parse()?;
            let message_id: i64 = status_id.parse()?;
            match self
                .api
                .call(
                    "editMessageText",
                    json!({"chat_id": chat_id, "message_id": message_id, "text": text}),
                )
                .await
            {
                Ok(_) => Ok(()),
                // The same words again is not a failure of the line.
                Err(error) if error.to_string().contains("message is not modified") => Ok(()),
                Err(error) => Err(error),
            }
        })
    }

    fn clear_status<'a>(
        &'a self,
        chat_id: &'a str,
        status_id: &'a str,
    ) -> ChannelFuture<'a, Result<()>> {
        Box::pin(async move {
            let chat_id: i64 = chat_id.parse()?;
            let message_id: i64 = status_id.parse()?;
            self.api
                .call(
                    "deleteMessage",
                    json!({"chat_id": chat_id, "message_id": message_id}),
                )
                .await?;
            Ok(())
        })
    }

    /// Telegram lets a bot delete its own messages, and other people's
    /// in a private chat for a while after they were sent; past that,
    /// or in a group where it is not an admin, it refuses, and the
    /// chat is told to delete the message itself.
    fn delete_message<'a>(
        &'a self,
        chat_id: &'a str,
        message_id: &'a str,
    ) -> ChannelFuture<'a, Result<bool>> {
        Box::pin(async move {
            let chat_id: i64 = chat_id.parse()?;
            let message_id: i64 = message_id.parse()?;
            match self
                .api
                .call(
                    "deleteMessage",
                    json!({"chat_id": chat_id, "message_id": message_id}),
                )
                .await
            {
                Ok(_) => Ok(true),
                Err(error) => {
                    log(&format!(
                        "telegram: message {message_id} not deleted ({error:#})"
                    ));
                    Ok(false)
                }
            }
        })
    }

    fn send<'a>(&'a self, message: Outbound) -> ChannelFuture<'a, Result<()>> {
        Box::pin(async move {
            let chat_id = Self::chat_id(&message)?;
            let markup = keyboard(&message.buttons);
            let mut pieces =
                crate::bus::split_for_delivery(&message.text, PIECE_LINES, PIECE_LINE_CHARS);
            // A text that fits a caption rides on the first file; a
            // longer one goes ahead of the files as messages of its own.
            let mut caption = None;
            if !message.media.is_empty()
                && pieces.len() == 1
                && pieces[0].chars().count() <= CAPTION_CHARS
            {
                caption = pieces.pop();
            }
            let piece_count = pieces.len();
            for (index, piece) in pieces.into_iter().enumerate() {
                let mut params = json!({"chat_id": chat_id, "text": piece});
                // The buttons go under the last thing sent.
                if index + 1 == piece_count
                    && message.media.is_empty()
                    && let Some(markup) = &markup
                {
                    params["reply_markup"] = markup.clone();
                }
                self.api.call("sendMessage", params).await?;
            }
            let media_count = message.media.len();
            for (index, path) in message.media.iter().enumerate() {
                let (method, field) = if is_image(path) {
                    ("sendPhoto", "photo")
                } else {
                    ("sendDocument", "document")
                };
                let mut fields = json!({"chat_id": chat_id});
                if let Some(caption) = caption.take() {
                    fields["caption"] = Value::String(caption);
                }
                if index + 1 == media_count
                    && let Some(markup) = &markup
                {
                    fields["reply_markup"] = markup.clone();
                }
                self.api.upload(method, fields, field, path).await?;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    type Calls = Arc<Mutex<Vec<(String, Value)>>>;

    /// The Bot API as a test sees it: records every call, answers the
    /// ones the adapter needs, and hands out update batches as the test
    /// pushes them — a poll waits for the next batch, as the real one
    /// waits on the server.
    struct FakeApi {
        calls: Calls,
        updates: tokio::sync::Mutex<mpsc::Receiver<Value>>,
        downloads: Mutex<Vec<(String, PathBuf)>>,
    }

    impl BotApi for FakeApi {
        fn call<'a>(&'a self, method: &'a str, params: Value) -> ChannelFuture<'a, Result<Value>> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .unwrap()
                    .push((method.to_string(), params.clone()));
                Ok(match method {
                    "getMe" => json!({"id": 99, "is_bot": true, "username": "ilar_bot"}),
                    "getUpdates" => match self.updates.lock().await.recv().await {
                        Some(batch) => batch,
                        None => {
                            // The test is over; block like a real long
                            // poll would until cancelled.
                            std::future::pending::<()>().await;
                            unreachable!()
                        }
                    },
                    "sendMessage" => json!({"message_id": 42}),
                    "getFile" => json!({"file_path": "photos/file_7.jpg"}),
                    _ => json!(true),
                })
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
                let mut recorded = fields.clone();
                recorded["_field"] = json!(field);
                recorded["_path"] = json!(path.display().to_string());
                self.calls
                    .lock()
                    .unwrap()
                    .push((method.to_string(), recorded));
                Ok(json!({"message_id": 43}))
            })
        }

        fn download<'a>(
            &'a self,
            file_path: &'a str,
            to: &'a Path,
        ) -> ChannelFuture<'a, Result<()>> {
            Box::pin(async move {
                std::fs::create_dir_all(to.parent().unwrap()).unwrap();
                std::fs::write(to, b"jpeg bytes").unwrap();
                self.downloads
                    .lock()
                    .unwrap()
                    .push((file_path.to_string(), to.to_path_buf()));
                Ok(())
            })
        }
    }

    fn fake() -> (Arc<FakeApi>, mpsc::Sender<Value>) {
        let (tx, rx) = mpsc::channel(8);
        let api = Arc::new(FakeApi {
            calls: Arc::new(Mutex::new(Vec::new())),
            updates: tokio::sync::Mutex::new(rx),
            downloads: Mutex::new(Vec::new()),
        });
        (api, tx)
    }

    fn user(id: i64, username: Option<&str>) -> Value {
        let mut user = json!({"id": id, "is_bot": false, "first_name": "Someone"});
        if let Some(name) = username {
            user["username"] = json!(name);
        }
        user
    }

    fn text_message(update_id: i64, from: Value, chat_id: i64, kind: &str, text: &str) -> Value {
        json!({
            "update_id": update_id,
            "message": {
                "message_id": update_id * 10,
                "from": from,
                "chat": {"id": chat_id, "type": kind},
                "text": text,
            }
        })
    }

    fn methods(calls: &Calls) -> Vec<String> {
        calls
            .lock()
            .unwrap()
            .iter()
            .map(|(m, _)| m.clone())
            .collect()
    }

    async fn next(inbound: &mut mpsc::Receiver<Inbound>) -> Inbound {
        tokio::time::timeout(std::time::Duration::from_secs(5), inbound.recv())
            .await
            .expect("a message in time")
            .expect("open")
    }

    /// An adapter running against the fake: what a test pushes in
    /// (`updates`), what comes out (`inbound`), and how it ends.
    struct Running {
        api: Arc<FakeApi>,
        updates: mpsc::Sender<Value>,
        inbound: mpsc::Receiver<Inbound>,
        cancel: CancellationToken,
        task: tokio::task::JoinHandle<Result<()>>,
    }

    impl Running {
        async fn stop(self) {
            self.cancel.cancel();
            let _ = self.task.await;
        }
    }

    fn started(config: TelegramConfig, dir: &Path) -> Running {
        let (api, updates) = fake();
        let channel = Telegram::over(config, api.clone(), dir);
        let (inbound_tx, inbound) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let run_cancel = cancel.clone();
        let task = tokio::spawn(async move { channel.run(inbound_tx, run_cancel).await });
        Running {
            api,
            updates,
            inbound,
            cancel,
            task,
        }
    }

    #[tokio::test]
    async fn messages_from_allowed_people_reach_the_bus_and_nothing_else_does() {
        let dir = tempfile::tempdir().unwrap();
        let mut run = started(
            TelegramConfig {
                allow_from: vec!["@Alice".into(), "777".into()],
                ack_reaction: Some("👀".into()),
                ..TelegramConfig::default()
            },
            dir.path(),
        );
        let batch = json!([
            text_message(1, user(1, Some("alice")), 1, "private", "hello bot"),
            text_message(2, user(2, Some("mallory")), 2, "private", "let me in"),
            text_message(3, json!({"id": 99, "is_bot": true, "username": "ilar_bot"}), 1, "private", "my own"),
            text_message(4, user(777, None), -100, "supergroup", "@ilar_bot what's up"),
            text_message(5, user(777, None), -100, "supergroup", "/new@ilar_bot"),
            {
                "update_id": 6,
                "message": {
                    "message_id": 60,
                    "from": user(1, Some("alice")),
                    "chat": {"id": 1, "type": "private"},
                    "caption": "look",
                    "photo": [
                        {"file_id": "small", "file_unique_id": "u1", "width": 90},
                        {"file_id": "big", "file_unique_id": "u1", "width": 1280},
                    ],
                }
            },
        ]);
        run.updates.send(batch).await.unwrap();

        let first = next(&mut run.inbound).await;
        assert_eq!(first.sender_id, "1");
        assert_eq!(first.chat_id, "1");
        assert_eq!(first.text, "hello bot");
        assert!(!first.is_group);
        assert_eq!(first.message_id.as_deref(), Some("10"));

        let group = next(&mut run.inbound).await;
        assert_eq!(group.chat_id, "-100");
        assert!(group.is_group);
        assert_eq!(group.text, "what's up", "the mention is stripped");
        let command = next(&mut run.inbound).await;
        assert_eq!(command.text, "/new", "the bot suffix is stripped");

        let photo = next(&mut run.inbound).await;
        assert_eq!(photo.text, "look");
        assert_eq!(photo.media.len(), 1);
        assert!(photo.media[0].exists(), "{:?}", photo.media);
        assert!(photo.media[0].starts_with(dir.path().join("telegram")));
        let downloads = run.api.downloads.lock().unwrap().clone();
        assert_eq!(downloads[0].0, "photos/file_7.jpg");
        let get_file = run
            .api
            .calls
            .lock()
            .unwrap()
            .iter()
            .find(|(m, _)| m == "getFile")
            .cloned()
            .unwrap();
        assert_eq!(get_file.1["file_id"], "big", "the largest size");

        // The stranger and the bot's own message never arrived.
        assert!(run.inbound.try_recv().is_err());
        let seen = methods(&run.api.calls);
        assert_eq!(&seen[..3], ["getMe", "setMyCommands", "getUpdates"]);
        assert!(
            seen.iter().filter(|m| *m == "setMessageReaction").count() >= 3,
            "{seen:?}"
        );
        run.stop().await;
    }

    #[tokio::test]
    async fn the_menu_is_set_from_the_command_list() {
        let dir = tempfile::tempdir().unwrap();
        let run = started(
            TelegramConfig {
                allow_anyone: true,
                ..TelegramConfig::default()
            },
            dir.path(),
        );
        for _ in 0..50 {
            if methods(&run.api.calls).iter().any(|m| m == "getUpdates") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let calls = run.api.calls.lock().unwrap().clone();
        let (_, params) = calls.iter().find(|(m, _)| m == "setMyCommands").unwrap();
        let names: Vec<&str> = params["commands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["command"].as_str().unwrap())
            .collect();
        for (name, _) in crate::commands::MENU {
            assert!(names.contains(name), "{name} missing from {names:?}");
        }
        run.stop().await;
    }

    #[tokio::test]
    async fn a_tapped_button_is_the_command_from_the_person_who_tapped() {
        let dir = tempfile::tempdir().unwrap();
        let mut run = started(
            TelegramConfig {
                allow_from: vec!["1".into()],
                ..TelegramConfig::default()
            },
            dir.path(),
        );
        let tap = |update_id: i64, from: Value, data: &str| {
            json!({
                "update_id": update_id,
                "callback_query": {
                    "id": format!("q{update_id}"),
                    "from": from,
                    "message": {"message_id": 42, "chat": {"id": 1, "type": "private"}},
                    "data": data,
                }
            })
        };
        run.updates
            .send(json!([
                tap(1, user(2, Some("mallory")), "/grant always"),
                tap(2, user(1, Some("alice")), "/grant session"),
            ]))
            .await
            .unwrap();
        let answer = next(&mut run.inbound).await;
        assert_eq!(answer.text, "/grant session");
        assert_eq!(answer.sender_id, "1");
        assert_eq!(answer.chat_id, "1");
        assert_eq!(answer.message_id, None);
        assert!(
            run.inbound.try_recv().is_err(),
            "the stranger's tap did nothing"
        );
        let calls = run.api.calls.lock().unwrap().clone();
        let answered: Vec<&Value> = calls
            .iter()
            .filter(|(m, _)| m == "answerCallbackQuery")
            .map(|(_, p)| p)
            .collect();
        assert_eq!(answered.len(), 2, "both taps stop their spinner");
        let bared: Vec<&Value> = calls
            .iter()
            .filter(|(m, _)| m == "editMessageReplyMarkup")
            .map(|(_, p)| p)
            .collect();
        assert_eq!(bared.len(), 1, "only the answered ask loses its keyboard");
        assert_eq!(bared[0]["message_id"], 42);
        run.stop().await;
    }

    #[tokio::test]
    async fn sends_chunk_under_the_cap_and_pick_the_method_by_file() {
        let dir = tempfile::tempdir().unwrap();
        let (api, _updates) = fake();
        let channel = Telegram::over(
            TelegramConfig {
                allow_anyone: true,
                ..TelegramConfig::default()
            },
            api.clone(),
            dir.path(),
        );
        let picture = dir.path().join("a.png");
        let file = dir.path().join("b.bin");
        std::fs::write(&picture, b"png").unwrap();
        std::fs::write(&file, b"bin").unwrap();
        let outbound = |text: String, media: Vec<PathBuf>, buttons: Vec<Button>| Outbound {
            channel: "telegram".into(),
            chat_id: "1".into(),
            text,
            media,
            buttons,
        };
        channel
            .send(outbound("plain".into(), vec![], vec![]))
            .await
            .unwrap();
        channel
            .send(outbound(
                "with a picture".into(),
                vec![picture.clone(), file.clone()],
                vec![],
            ))
            .await
            .unwrap();
        // 6,000 characters of prose: over the cap, so more than one.
        let long = (0..60)
            .map(|_| "w".repeat(99))
            .collect::<Vec<_>>()
            .join("\n");
        channel.send(outbound(long, vec![], vec![])).await.unwrap();
        // A caption too long for one goes ahead as messages of its own.
        let caption_too_long = "c".repeat(2_000);
        channel
            .send(outbound(caption_too_long, vec![picture.clone()], vec![]))
            .await
            .unwrap();
        channel
            .send(outbound(
                "choose".into(),
                vec![],
                vec![Button::new("Once", "/grant"), Button::new("Deny", "/deny")],
            ))
            .await
            .unwrap();

        let calls = api.calls.lock().unwrap().clone();
        assert_eq!(calls[0].0, "sendMessage");
        assert_eq!(calls[0].1["text"], "plain");
        assert_eq!(calls[1].0, "sendPhoto");
        assert_eq!(calls[1].1["caption"], "with a picture");
        assert_eq!(calls[1].1["_field"], "photo");
        assert_eq!(calls[2].0, "sendDocument");
        assert!(calls[2].1.get("caption").is_none());
        let chunks: Vec<&(String, Value)> = calls[3..]
            .iter()
            .take_while(|(m, p)| {
                m == "sendMessage" && p["text"].as_str().unwrap().starts_with("www")
            })
            .collect();
        assert!(chunks.len() >= 2, "{}", chunks.len());
        for (_, params) in &chunks {
            assert!(params["text"].as_str().unwrap().chars().count() <= 4096);
        }
        let after = 3 + chunks.len();
        assert_eq!(calls[after].0, "sendMessage", "the long caption goes first");
        assert!(calls[after].1["text"].as_str().unwrap().starts_with("ccc"));
        assert_eq!(calls[after + 1].0, "sendPhoto");
        assert!(calls[after + 1].1.get("caption").is_none());
        let last = calls.last().unwrap();
        assert_eq!(last.0, "sendMessage");
        let keyboard = &last.1["reply_markup"]["inline_keyboard"];
        assert_eq!(keyboard[0][0]["text"], "Once");
        assert_eq!(keyboard[0][0]["callback_data"], "/grant");
        assert_eq!(keyboard[0][1]["callback_data"], "/deny");
    }

    #[tokio::test]
    async fn an_open_allow_list_refuses_to_start() {
        let dir = tempfile::tempdir().unwrap();
        let (api, _updates) = fake();
        let channel = Telegram::over(TelegramConfig::default(), api, dir.path());
        let (inbound_tx, _inbound) = mpsc::channel(1);
        let error = channel
            .run(inbound_tx, CancellationToken::new())
            .await
            .expect_err("an empty allow_from must not start");
        assert!(error.to_string().contains("allow_from"), "{error:#}");
    }

    #[test]
    fn mentions_are_stripped_and_names_are_kept_safe() {
        assert_eq!(strip_mention("/new@ilar_bot", "ilar_bot"), "/new");
        assert_eq!(
            strip_mention("/model@ilar_bot zai/glm-4.7", "ilar_bot"),
            "/model zai/glm-4.7"
        );
        assert_eq!(strip_mention("@ilar_bot hello", "ilar_bot"), "hello");
        assert_eq!(strip_mention("@Ilar_Bot hello", "ilar_bot"), "hello");
        assert_eq!(
            strip_mention("hello @ilar_bot", "ilar_bot"),
            "hello @ilar_bot"
        );
        assert_eq!(
            strip_mention("/new@other_bot", "ilar_bot"),
            "/new@other_bot"
        );
        assert_eq!(strip_mention("/new", ""), "/new");
        assert_eq!(safe_name("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(safe_name("report v2.pdf"), "report_v2.pdf");
        assert_eq!(safe_name("..."), "file");
    }

    #[test]
    fn the_keyboard_leaves_off_what_telegram_would_refuse() {
        let long = "/approve ".to_string() + &"x".repeat(80);
        let markup =
            keyboard(&[Button::new("Fine", "/deny"), Button::new("Too long", &long)]).unwrap();
        assert_eq!(markup["inline_keyboard"][0].as_array().unwrap().len(), 1);
        assert!(keyboard(&[]).is_none());
        // Two to a row.
        let four = keyboard(&crate::grants::grant_buttons()).unwrap();
        assert_eq!(four["inline_keyboard"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn the_constraints_name_the_cap_the_code_splits_at() {
        let dir = tempfile::tempdir().unwrap();
        let (api, _updates) = fake();
        let channel = Telegram::over(TelegramConfig::default(), api, dir.path());
        let text = channel.constraints().to_string();
        assert!(text.contains(&format!("{PIECE_LINES} lines")), "{text}");
        assert!(
            text.contains(&format!("{PIECE_LINE_CHARS} characters")),
            "{text}"
        );
    }
}
