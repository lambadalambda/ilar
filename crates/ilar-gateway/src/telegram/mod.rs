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
#[derive(Debug, Clone, Deserialize, PartialEq)]
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
    /// In a group, answer only what is addressed to the bot: a
    /// mention, a reply to one of its messages, or a command. On by
    /// default; off, the bot answers everything a group says.
    #[serde(default = "yes")]
    pub group_mention_only: bool,
}

fn yes() -> bool {
    true
}

impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            token: None,
            allow_from: Vec::new(),
            allow_anyone: false,
            ack_reaction: None,
            media_dir: None,
            group_mention_only: true,
        }
    }
}

/// How long one `getUpdates` waits on the server. The API allows 50.
const POLL_SECS: u64 = 30;
/// After a failed poll — the network is down, Telegram is not — before
/// the next one; doubled per failure up to the cap.
const POLL_RETRY: std::time::Duration = std::time::Duration::from_secs(5);
const POLL_RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(60);
/// Telegram's cap on a message is 4096 characters and on a caption
/// 1024. Pieces are cut by the gateway's line-counting splitter at 36
/// lines of 100 — at most 3,600 characters and 36 line breaks — with
/// room to spare in case the cap counts wider than code points.
const PIECE_LINES: usize = 36;
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
    /// The bot's own id, for telling a reply to one of its messages.
    bot_id: std::sync::atomic::AtomicI64,
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
            bot_id: std::sync::atomic::AtomicI64::new(0),
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
        *self.username.lock().unwrap() = Some(username.clone());
        self.bot_id.store(
            me.get("id").and_then(Value::as_i64).unwrap_or(0),
            std::sync::atomic::Ordering::Release,
        );
        // The menu: what typing `/` offers. Best effort — a menu that
        // would not set is a bot without one, not a bot that is down.
        let entries = |menu: &mut dyn Iterator<Item = (&str, &str)>| -> Vec<Value> {
            menu.map(|(name, description)| json!({"command": name, "description": description}))
                .collect()
        };
        // Groups get their own, smaller menu: a room is refused what
        // reaches past it, so it is not offered it either.
        let menus = [
            json!({"commands": entries(&mut crate::commands::MENU.iter().copied())}),
            json!({
                "commands": entries(&mut crate::commands::group_menu()),
                "scope": {"type": "all_group_chats"},
            }),
        ];
        for menu in menus {
            if let Err(error) = self.api.call("setMyCommands", menu).await {
                log(&format!(
                    "telegram: the command menu was not set: {error:#}"
                ));
            }
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
        let message_id = message.get("message_id").and_then(Value::as_i64);
        let raw = message
            .get("text")
            .or_else(|| message.get("caption"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let username = self.username.lock().unwrap().clone().unwrap_or_default();
        let mut text = strip_mention(raw, &username);
        // A group talks among itself; the bot answers what is said to
        // it — a mention, a reply to its own message, a command — and
        // lets the rest go by without a word.
        if is_group && self.config.group_mention_only {
            let mentioned = text != raw.trim();
            let command = raw.trim_start().starts_with('/');
            let replied_to = message
                .pointer("/reply_to_message/from/id")
                .and_then(Value::as_i64)
                .is_some_and(|id| id == self.bot_id.load(std::sync::atomic::Ordering::Acquire));
            if !(mentioned || command || replied_to) {
                return Ok(());
            }
        }
        // A file that cannot be fetched — too big for the API, a
        // network blip — does not take the person's words with it: the
        // text goes on, with a note where the file would have been.
        let media = match attachment(message) {
            Some(file) => match self.fetch(&file).await {
                Ok(path) => vec![path],
                Err(error) => {
                    log(&format!("telegram: attachment not fetched: {error:#}"));
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(
                        "(an attachment came with this message but could not be fetched)",
                    );
                    Vec::new()
                }
            },
            None => Vec::new(),
        };
        if text.trim().is_empty() && media.is_empty() {
            return Ok(());
        }
        if let (Some(emoji), Some(id)) = (&self.config.ack_reaction, message_id) {
            let reaction = json!({
                "chat_id": chat_id,
                "message_id": id,
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
                sender_name: display_name(from),
                message_id: message_id.map(|id| id.to_string()),
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
                sender_name: display_name(from),
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
/// cannot leave the media directory. Letters in any script stay, so
/// a name a person can read is still one.
fn safe_name(name: &str) -> String {
    let kept: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '.' | '-' | '_') {
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

/// `/new@ilar_bot` is `/new`; `hey @ilar_bot, what's up` is `hey,
/// what's up`. Telegram addresses a bot in a group both ways, and a
/// person mentions it wherever the sentence puts it. The mention has
/// to be a whole word — `@ilar_botty` is somebody else — and the text
/// is walked by characters, never sliced by bytes: a message in
/// Cyrillic or emoji has byte lengths that are nobody's business.
fn strip_mention(text: &str, username: &str) -> String {
    let text = text.trim();
    if username.is_empty() {
        return text.to_string();
    }
    let (first, rest) = match text.split_once(char::is_whitespace) {
        Some((first, rest)) => (first, rest.trim_start()),
        None => (text, ""),
    };
    let body = match first.strip_prefix('/') {
        // `/new@ilar_bot` — the mention is glued to the command.
        Some(command) => match command.rsplit_once('@') {
            Some((name, at)) if !name.is_empty() && at.eq_ignore_ascii_case(username) => {
                if rest.is_empty() {
                    format!("/{name}")
                } else {
                    format!("/{name} {rest}")
                }
            }
            _ => text.to_string(),
        },
        None => text.to_string(),
    };
    // Then the mention as a word of its own, anywhere. ASCII lowering
    // keeps every byte offset where it was, so the offsets found in
    // the lowered copy index the original.
    let needle = format!("@{}", username.to_ascii_lowercase());
    let lowered = body.to_ascii_lowercase();
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = String::with_capacity(body.len());
    let mut at = 0;
    while let Some(found) = lowered[at..].find(&needle) {
        let start = at + found;
        let end = start + needle.len();
        let bounded_before = lowered[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !is_word(c));
        let bounded_after = lowered[end..].chars().next().is_none_or(|c| !is_word(c));
        if bounded_before && bounded_after {
            out.push_str(&body[at..start]);
            // One space goes with the mention, so "hey @bot what" is
            // "hey what": the one after it, or, before a comma or the
            // end, the one before it.
            if body[end..].starts_with(' ') && !out.is_empty() {
                at = end + 1;
            } else {
                if out.ends_with(' ') {
                    out.pop();
                }
                at = end;
            }
        } else {
            out.push_str(&body[at..end]);
            at = end;
        }
    }
    out.push_str(&body[at..]);
    out.trim().to_string()
}

/// What a person is called: first and last name as Telegram has them,
/// else the username, else nothing.
fn display_name(user: &Value) -> Option<String> {
    let first = user.get("first_name").and_then(Value::as_str).unwrap_or("");
    let last = user.get("last_name").and_then(Value::as_str).unwrap_or("");
    let name = format!("{first} {last}").trim().to_string();
    if !name.is_empty() {
        return Some(name);
    }
    user.get("username")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Whether an update from before this start would act on the present:
/// a command, or a tapped button. A `/grant always` tapped hours ago
/// must not answer whatever ask is standing now, and an `/unlock` the
/// adapter deleted from the chat is still in Telegram's backlog.
fn is_stale_command(update: &Value) -> bool {
    if update.get("callback_query").is_some() {
        return true;
    }
    update
        .pointer("/message/text")
        .and_then(Value::as_str)
        .is_some_and(|text| text.trim_start().starts_with('/'))
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

/// Which upload a file is: Telegram shows a gif sent as a photo as a
/// still, so it goes as an animation.
fn upload_kind(path: &Path) -> (&'static str, &'static str) {
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase());
    if ext.as_deref() == Some("gif") {
        ("sendAnimation", "animation")
    } else if crate::channel::is_image(path) {
        ("sendPhoto", "photo")
    } else {
        ("sendDocument", "document")
    }
}

impl Channel for Telegram {
    fn name(&self) -> &str {
        "telegram"
    }

    fn constraints(&self) -> &str {
        "plain text, no markdown rendering — asterisks and underscores show as typed; a long \
         text is sent as several messages, each under 36 lines of 100 characters, split at line \
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
            // Telegram keeps what arrived while the gateway was down.
            // The first poll takes it without waiting, and the commands
            // in it are dropped — see [`is_stale_command`]; the
            // messages are answered.
            let mut backlog = true;
            let mut retry = POLL_RETRY;
            loop {
                let mut params = json!({
                    "timeout": if backlog { 0 } else { POLL_SECS },
                    "allowed_updates": ["message", "callback_query"],
                });
                if let Some(offset) = offset {
                    params["offset"] = json!(offset);
                }
                let updates = tokio::select! {
                    () = cancel.cancelled() => return Ok(()),
                    updates = self.api.call("getUpdates", params) => updates,
                };
                let updates = match updates {
                    Ok(updates) => {
                        retry = POLL_RETRY;
                        updates
                    }
                    Err(error) => {
                        // The network, most likely: wait and ask again
                        // rather than restart the channel for it. A
                        // failure that stays — the token revoked, a
                        // second gateway on the same bot — backs off
                        // to a line a minute rather than one every
                        // five seconds.
                        log(&format!("telegram: poll failed: {error:#}"));
                        tokio::select! {
                            () = cancel.cancelled() => return Ok(()),
                            () = tokio::time::sleep(retry) => {}
                        }
                        retry = (retry * 2).min(POLL_RETRY_MAX);
                        continue;
                    }
                };
                for update in updates.as_array().into_iter().flatten() {
                    if let Some(id) = update.get("update_id").and_then(Value::as_i64) {
                        offset = Some(offset.map_or(id + 1, |current| current.max(id + 1)));
                    }
                    if backlog && is_stale_command(update) {
                        log("telegram: a command from before the start was dropped");
                        continue;
                    }
                    if let Err(error) = self.update(update, &inbound).await {
                        log(&format!("telegram: update dropped: {error:#}"));
                    }
                }
                backlog = false;
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
                // Quiet: a line that changes every few seconds must
                // not buzz the phone each time.
                .call(
                    "sendMessage",
                    json!({"chat_id": chat_id, "text": text, "disable_notification": true}),
                )
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
                let (method, field) = upload_kind(path);
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
        /// Methods to refuse, once each, with the description given —
        /// what a channel that is down, or a message that is gone,
        /// answers.
        failing: Mutex<Vec<(String, String)>>,
    }

    impl FakeApi {
        fn fail_next(&self, method: &str, description: &str) {
            self.failing
                .lock()
                .unwrap()
                .push((method.to_string(), description.to_string()));
        }

        fn refusal(&self, method: &str) -> Option<anyhow::Error> {
            let mut failing = self.failing.lock().unwrap();
            let at = failing.iter().position(|(m, _)| m == method)?;
            let (_, description) = failing.remove(at);
            Some(anyhow::anyhow!("{method}: {description}"))
        }
    }

    impl BotApi for FakeApi {
        fn call<'a>(&'a self, method: &'a str, params: Value) -> ChannelFuture<'a, Result<Value>> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .unwrap()
                    .push((method.to_string(), params.clone()));
                if let Some(error) = self.refusal(method) {
                    return Err(error);
                }
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
            failing: Mutex::new(Vec::new()),
        });
        (api, tx)
    }

    /// What piled up while the gateway was down is read once without
    /// waiting, and its commands and taps are dropped: a `/grant
    /// always` from hours ago must not answer the ask standing now.
    /// The messages in it are answered, and everything after the first
    /// poll is live.
    #[tokio::test]
    async fn stale_commands_in_the_backlog_are_dropped_and_messages_are_not() {
        let dir = tempfile::tempdir().unwrap();
        let mut run = started(
            TelegramConfig {
                allow_from: vec!["1".into()],
                ..TelegramConfig::default()
            },
            dir.path(),
        );
        let tap = json!({
            "update_id": 2,
            "callback_query": {
                "id": "q2",
                "from": user(1, Some("alice")),
                "message": {"message_id": 42, "chat": {"id": 1, "type": "private"}},
                "data": "/grant always",
            }
        });
        run.updates
            .send(json!([
                text_message(1, user(1, Some("alice")), 1, "private", "/unlock hunter2"),
                tap,
                text_message(3, user(1, Some("alice")), 1, "private", "still here?"),
            ]))
            .await
            .unwrap();
        let live = next(&mut run.inbound).await;
        assert_eq!(live.text, "still here?");
        // The second poll is live: a command in it is a command.
        run.updates
            .send(json!([text_message(
                4,
                user(1, Some("alice")),
                1,
                "private",
                "/new"
            )]))
            .await
            .unwrap();
        let command = next(&mut run.inbound).await;
        assert_eq!(command.text, "/new");
        assert!(run.inbound.try_recv().is_err());
        let calls = run.api.calls.lock().unwrap().clone();
        let polls: Vec<&Value> = calls
            .iter()
            .filter(|(m, _)| m == "getUpdates")
            .map(|(_, p)| p)
            .collect();
        assert_eq!(
            polls[0]["timeout"], 0,
            "the backlog is taken without waiting"
        );
        assert!(
            polls[0].get("offset").is_none(),
            "no offset means no null offset"
        );
        assert_eq!(polls[1]["timeout"], POLL_SECS);
        assert_eq!(polls[1]["offset"], 4, "past the backlog");
        run.stop().await;
    }

    /// The refusals that are not failures: the same status text again
    /// is fine, a message Telegram will not delete is a `false`, and a
    /// file that will not fetch still lets the words through.
    #[tokio::test]
    async fn refusals_are_handled_where_the_trait_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let mut run = started(
            TelegramConfig {
                allow_from: vec!["1".into()],
                ..TelegramConfig::default()
            },
            dir.path(),
        );
        // Let setup finish before pushing refusals at the API.
        run.updates.send(json!([])).await.unwrap();
        for _ in 0..50 {
            if methods(&run.api.calls).iter().any(|m| m == "getUpdates") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let channel = Telegram::over(
            TelegramConfig {
                allow_anyone: true,
                ..TelegramConfig::default()
            },
            run.api.clone(),
            dir.path(),
        );
        run.api
            .fail_next("editMessageText", "Bad Request: message is not modified");
        channel.edit_status("1", "42", "working…").await.unwrap();
        run.api
            .fail_next("deleteMessage", "Bad Request: message can't be deleted");
        assert!(!channel.delete_message("1", "7").await.unwrap());
        run.api.fail_next("getFile", "Bad Request: file is too big");
        run.updates
            .send(json!([{
                "update_id": 9,
                "message": {
                    "message_id": 90,
                    "from": user(1, Some("alice")),
                    "chat": {"id": 1, "type": "private"},
                    "caption": "the report",
                    "document": {"file_id": "d1", "file_unique_id": "u9", "file_name": "big.pdf"},
                }
            }]))
            .await
            .unwrap();
        let message = next(&mut run.inbound).await;
        assert!(message.text.starts_with("the report\n"), "{}", message.text);
        assert!(
            message.text.contains("could not be fetched"),
            "{}",
            message.text
        );
        assert!(message.media.is_empty());
        run.stop().await;
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
        // An empty backlog: what follows is live, commands included.
        run.updates.send(json!([])).await.unwrap();
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
        assert_eq!(first.sender_name.as_deref(), Some("Someone"));

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
        assert_eq!(
            &seen[..4],
            ["getMe", "setMyCommands", "setMyCommands", "getUpdates"]
        );
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
        // And a smaller one for groups, without what a room is refused.
        let (_, group) = calls
            .iter()
            .find(|(m, p)| m == "setMyCommands" && p["scope"]["type"] == "all_group_chats")
            .expect("a group menu");
        let offered: Vec<&str> = group["commands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["command"].as_str().unwrap())
            .collect();
        assert!(offered.contains(&"new"), "{offered:?}");
        assert!(!offered.contains(&"unlock"), "{offered:?}");
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
        // An empty backlog first: a tap in the backlog is stale by
        // definition and dropped, which is its own test.
        run.updates.send(json!([])).await.unwrap();
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

    /// A group talks among itself. What is said to the bot — a
    /// mention anywhere, a reply to its message, a command — arrives;
    /// the rest does not. Off, everything arrives.
    #[tokio::test]
    async fn in_a_group_only_what_is_said_to_the_bot_arrives() {
        let dir = tempfile::tempdir().unwrap();
        let group = |update_id: i64, text: &str, reply_to_bot: bool| {
            let mut message = json!({
                "message_id": update_id * 10,
                "from": user(777, Some("carol")),
                "chat": {"id": -100, "type": "supergroup"},
                "text": text,
            });
            if reply_to_bot {
                message["reply_to_message"] = json!({
                    "message_id": 5,
                    "from": {"id": 99, "is_bot": true, "username": "ilar_bot"},
                    "text": "earlier answer",
                });
            }
            json!({"update_id": update_id, "message": message})
        };
        let mut run = started(
            TelegramConfig {
                allow_from: vec!["777".into()],
                ..TelegramConfig::default()
            },
            dir.path(),
        );
        run.updates.send(json!([])).await.unwrap();
        run.updates
            .send(json!([
                group(1, "morning everyone", false),
                group(2, "hey @ilar_bot, what's up", false),
                group(3, "and this?", true),
                group(4, "/status@ilar_bot", false),
                group(5, "/status", false),
                group(6, "nobody asked you", false),
            ]))
            .await
            .unwrap();
        let mentioned = next(&mut run.inbound).await;
        assert_eq!(mentioned.text, "hey, what's up");
        assert!(mentioned.is_group);
        assert_eq!(mentioned.sender_name.as_deref(), Some("Someone"));
        assert_eq!(next(&mut run.inbound).await.text, "and this?");
        assert_eq!(next(&mut run.inbound).await.text, "/status");
        assert_eq!(next(&mut run.inbound).await.text, "/status");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(run.inbound.try_recv().is_err(), "the chatter went by");
        run.stop().await;

        // A group that is the bot's own: everything is for it.
        let mut run = started(
            TelegramConfig {
                allow_from: vec!["777".into()],
                group_mention_only: false,
                ..TelegramConfig::default()
            },
            dir.path(),
        );
        run.updates.send(json!([])).await.unwrap();
        run.updates
            .send(json!([group(7, "morning everyone", false)]))
            .await
            .unwrap();
        assert_eq!(next(&mut run.inbound).await.text, "morning everyone");
        run.stop().await;
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
        assert_eq!(strip_mention("hello @ilar_bot", "ilar_bot"), "hello");
        // Anywhere in the sentence, as a whole word.
        assert_eq!(
            strip_mention("hey @ilar_bot, what's up", "ilar_bot"),
            "hey, what's up"
        );
        assert_eq!(
            strip_mention("hey @Ilar_Bot what's up @ilar_bot", "ilar_bot"),
            "hey what's up"
        );
        assert_eq!(
            strip_mention("mail me at me@ilar_bot.example", "ilar_bot"),
            "mail me at me@ilar_bot.example"
        );
        assert_eq!(
            strip_mention("/new@other_bot", "ilar_bot"),
            "/new@other_bot"
        );
        assert_eq!(strip_mention("/new", ""), "/new");
        // Walked by characters: a byte slice at the mention's length
        // used to panic inside a Cyrillic or emoji message, and take
        // the channel down with it.
        assert_eq!(strip_mention("Привет", "ilar_bot"), "Привет");
        assert_eq!(strip_mention("😀😀😀", "ilar_bot"), "😀😀😀");
        assert_eq!(strip_mention("@ilar_bot привет", "ilar_bot"), "привет");
        // The whole first word, or nothing: another bot's name that
        // begins the same is not a mention of this one.
        assert_eq!(
            strip_mention("@ilar_botty hi", "ilar_bot"),
            "@ilar_botty hi"
        );
        assert_eq!(strip_mention("@ilar_bot", "ilar_bot"), "");
        assert_eq!(safe_name("報告 v2.pdf"), "報告_v2.pdf");
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
