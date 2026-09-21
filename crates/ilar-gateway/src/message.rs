//! The `message` tool: how the model talks to a chat.
//!
//! A reply is not the turn's final text but a call the model makes,
//! so it can send several, send to another chat, or send nothing.
//! The turn's final text is delivered only when the model sent
//! nothing itself.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ilar::tools::{
    Tool, ToolConcurrency, ToolContext, ToolFuture, ToolOutput, WorkspaceAccess, parse_input,
};
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::bus::{Outbound, session_key};
use crate::driver::log;
use crate::routes::RouteStore;

/// How long taking the chat's status line down may hold the tool call.
/// It goes over the channel's rpc, and a wedged one hung the turn here
/// — a line left standing a few seconds is cosmetic, a turn that never
/// returns is not.
const CLEAR_STATUS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Deserialize)]
struct Input {
    text: String,
    /// Another chat than the one this turn belongs to.
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    chat: Option<String>,
    /// Files to attach, by path.
    #[serde(default)]
    media: Vec<PathBuf>,
    /// The text speaks of an attachment on purpose and there is none.
    #[serde(default)]
    no_attachment: bool,
}

/// What the tool needs to send for one seat.
pub struct Sending {
    pub outbound: mpsc::Sender<Outbound>,
    /// The seat's own chat.
    pub channel: String,
    pub chat_id: String,
    /// The seat's key. A reply to its own chat takes that chat's
    /// status line down, and a background seat's key is no chat's, so
    /// a scheduled turn's message never takes a line the person is
    /// watching down with it.
    pub key: String,
    pub routes: Arc<RouteStore>,
    /// Where a relative media path is looked for.
    pub workspace: PathBuf,
    /// What the model is told about sending here.
    pub constraints: String,
    pub status: Arc<crate::status::StatusBoard>,
}

pub struct MessageTool {
    sending: Sending,
    /// Messages sent during the current turn; the driver reads it to
    /// decide whether the final text still needs delivering.
    sent: Arc<AtomicUsize>,
    /// Built per seat, since it names the chat and carries the
    /// channel's constraints; leaked once because the trait hands out
    /// a static string.
    description: &'static str,
}

/// The chat a call addresses: the turn's own unless named. A session
/// key passed as the channel is taken apart rather than doubled.
fn address(
    channel: Option<String>,
    chat: Option<String>,
    home_channel: &str,
    home_chat: &str,
) -> Result<(String, String), String> {
    // A session key in `chat` is the same slip as one in `channel`.
    let (channel, chat) = match chat {
        Some(key) if key.contains(':') => {
            let (name, id) = key.split_once(':').unwrap_or((&key, ""));
            match channel {
                Some(given) if given != name && given != key => {
                    return Err(format!(
                        "chat is a chat's id, not a session key; {key:?} is on {name} but \
                         channel says {given:?}"
                    ));
                }
                _ => (Some(name.to_string()), Some(id.to_string())),
            }
        }
        other => (channel, other),
    };
    match (channel, chat) {
        (None, None) => Ok((home_channel.to_string(), home_chat.to_string())),
        (None, Some(chat)) => Ok((home_channel.to_string(), chat)),
        (Some(channel), chat) => {
            if let Some((name, id)) = channel.split_once(':') {
                return match chat {
                    Some(chat) if chat != id => Err(format!(
                        "channel is a channel's name ({home_channel}), not a session key; \
                         {channel:?} names chat {id} but chat says {chat:?}"
                    )),
                    _ => Ok((name.to_string(), id.to_string())),
                };
            }
            match chat {
                Some(chat) => Ok((channel, chat)),
                None if channel == home_channel => Ok((channel, home_chat.to_string())),
                None => Err(format!(
                    "chat is needed with channel {channel:?}: which chat on it?"
                )),
            }
        }
    }
}

/// Whether the text tells the reader something is attached.
fn claims_attachment(text: &str) -> bool {
    let lower = text.to_lowercase();
    ["attached", "attachment", "attaching"]
        .iter()
        .any(|word| lower.contains(word))
}

impl MessageTool {
    pub fn new(sending: Sending) -> (Arc<Self>, Arc<AtomicUsize>) {
        let sent = Arc::new(AtomicUsize::new(0));
        let home_channel = &sending.channel;
        let description = format!(
            "Send a message to the person you are talking to, on {home_channel}. Call it for \
             every reply you want them to see; your final text is delivered only if you sent \
             nothing this turn. Another known chat is named with channel (the channel's name, \
             {home_channel}) and chat (its id). Files travel in media, by path: the text alone \
             attaches nothing, and a text that speaks of an attachment without one is refused \
             unless no_attachment is true. A text that names pictures carries them in the same \
             call: never the caption first and the files in a call after.{}",
            if sending.constraints.is_empty() {
                String::new()
            } else {
                format!(" Delivery constraints: {}", sending.constraints)
            }
        );
        let tool = Arc::new(Self {
            sending,
            sent: sent.clone(),
            description: Box::leak(description.into_boxed_str()),
        });
        (tool, sent)
    }
}

impl Tool for MessageTool {
    fn name(&self) -> &'static str {
        "message"
    }

    fn description(&self) -> &'static str {
        self.description
    }

    fn concurrency(&self) -> ToolConcurrency {
        // Sequential, so two sends in one response arrive in order.
        ToolConcurrency::Barrier
    }

    fn workspace_access(&self) -> WorkspaceAccess {
        WorkspaceAccess::None
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "text": {"type": "string", "description": "What to send"},
                "channel": {"type": "string", "description": "Another chat's channel (default: this chat's)"},
                "chat": {"type": "string", "description": "Another chat's id (default: this chat)"},
                "media": {"type": "array", "items": {"type": "string"}, "description": "Files to attach, by path (relative to the workspace or absolute); images arrive as pictures"},
                "no_attachment": {"type": "boolean", "description": "Send a text that mentions an attachment without one, on purpose"}
            },
            "required": ["text"]
        })
    }

    fn run(&self, input: serde_json::Value, _ctx: ToolContext) -> ToolFuture {
        // The future outlives this borrow: everything it needs is cloned.
        let outbound = self.sending.outbound.clone();
        let home_channel = self.sending.channel.clone();
        let home_chat = self.sending.chat_id.clone();
        let home_key = self.sending.key.clone();
        let status = self.sending.status.clone();
        let routes = self.sending.routes.clone();
        let workspace = self.sending.workspace.clone();
        let sent = self.sent.clone();
        Box::pin(async move {
            let input: Input = match parse_input(input, "message") {
                Ok(input) => input,
                Err(error) => return error,
            };
            let (channel, chat_id) =
                match address(input.channel, input.chat, &home_channel, &home_chat) {
                    Ok(address) => address,
                    Err(why) => return ToolOutput::error(format!("message: {why}")),
                };
            let key = session_key(&channel, &chat_id);
            // Only chats that have written are addressable: the model
            // does not get to open conversations with strangers.
            let routes = routes.snapshot();
            if routes.session_for(&key).is_none() {
                return ToolOutput::error(format!(
                    "message: no chat {key}; only chats that have written to you can be \
                     messaged. Known: {}",
                    routes.known_chats()
                ));
            }
            if input.text.trim().is_empty() && input.media.is_empty() {
                return ToolOutput::error("message: nothing to send");
            }
            if input.media.is_empty() && !input.no_attachment && claims_attachment(&input.text) {
                return ToolOutput::error(
                    "message: the text speaks of an attachment but media is empty; put the \
                     file's path in media, or send with no_attachment: true if that is meant",
                );
            }
            // A channel sends what it is given by absolute path, and
            // a path that is not there is an error now rather than a
            // silent nothing later.
            let mut media = Vec::with_capacity(input.media.len());
            for path in input.media {
                let resolved = if path.is_absolute() {
                    path
                } else {
                    workspace.join(path)
                };
                if !resolved.is_file() {
                    return ToolOutput::error(format!(
                        "message: no file at {}",
                        resolved.display()
                    ));
                }
                media.push(resolved);
            }
            let message = Outbound {
                channel,
                chat_id,
                text: input.text,
                media,
                buttons: Vec::new(),
            };
            // This chat's status line was waiting for exactly this, so
            // it comes down before the reply does — and only this
            // seat's own line: a message to another chat leaves that
            // chat's running turn alone.
            //
            // Bounded, because this runs inside the tool call: taking
            // the line down goes over the channel's rpc, and a wedged
            // one hung the whole turn here. A line left standing for a
            // few seconds is a cosmetic fault; a turn that never
            // returns is not.
            if key == home_key
                && tokio::time::timeout(CLEAR_STATUS_TIMEOUT, status.clear(&home_key))
                    .await
                    .is_err()
            {
                log(&format!(
                    "{home_key}: status line not cleared in time; sending anyway"
                ));
            }
            if outbound.send(message).await.is_err() {
                return ToolOutput::error("message: the gateway is not delivering");
            }
            sent.fetch_add(1, Ordering::AcqRel);
            ToolOutput::text(format!("sent to {key}"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ilar::tools::Tool;

    fn tool(dir: &std::path::Path) -> (Arc<MessageTool>, mpsc::Receiver<Outbound>) {
        let board =
            crate::status::StatusBoard::new(Default::default(), false, std::time::Duration::ZERO);
        tool_on(dir, "fake:12", board)
    }

    /// The tool as one seat has it: its key, and the board its replies
    /// may take a line down on.
    fn tool_on(
        dir: &std::path::Path,
        key: &str,
        status: Arc<crate::status::StatusBoard>,
    ) -> (Arc<MessageTool>, mpsc::Receiver<Outbound>) {
        let (tx, rx) = mpsc::channel(4);
        let routes = Arc::new(RouteStore::open(dir.join("routes.json")).unwrap());
        routes
            .update(|routes| {
                routes.bind("fake:12", "s1");
                routes.bind("fake:15", "s2");
            })
            .unwrap();
        let (tool, _) = MessageTool::new(Sending {
            outbound: tx,
            channel: "fake".into(),
            chat_id: "12".into(),
            key: key.to_string(),
            routes,
            workspace: dir.to_path_buf(),
            constraints: String::new(),
            status,
        });
        (tool, rx)
    }

    async fn send(
        tool: &MessageTool,
        dir: &std::path::Path,
        input: serde_json::Value,
    ) -> ToolOutput {
        tool.run(input, ToolContext::root(dir.to_path_buf())).await
    }

    #[test]
    fn a_session_key_as_the_channel_is_taken_apart_not_doubled() {
        let home = ("fake", "12");
        let ok = |channel: Option<&str>, chat: Option<&str>| {
            address(
                channel.map(String::from),
                chat.map(String::from),
                home.0,
                home.1,
            )
        };
        assert_eq!(ok(None, None).unwrap(), ("fake".into(), "12".into()));
        assert_eq!(ok(None, Some("15")).unwrap(), ("fake".into(), "15".into()));
        assert_eq!(
            ok(Some("fake:15"), None).unwrap(),
            ("fake".into(), "15".into())
        );
        assert_eq!(
            ok(Some("fake:15"), Some("15")).unwrap(),
            ("fake".into(), "15".into())
        );
        assert_eq!(
            ok(Some("fake"), None).unwrap(),
            ("fake".into(), "12".into())
        );
        assert!(
            ok(Some("fake:15"), Some("12"))
                .unwrap_err()
                .contains("not a session key")
        );
        assert_eq!(
            ok(None, Some("fake:15")).unwrap(),
            ("fake".into(), "15".into())
        );
        assert_eq!(
            ok(Some("fake"), Some("fake:15")).unwrap(),
            ("fake".into(), "15".into())
        );
        assert!(
            ok(Some("other"), Some("fake:15"))
                .unwrap_err()
                .contains("not a session key")
        );
        assert!(
            ok(Some("other"), None)
                .unwrap_err()
                .contains("chat is needed")
        );
    }

    #[tokio::test]
    async fn nonsense_is_refused_with_the_fix_and_good_sends_go_out() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, mut rx) = tool(dir.path());
        std::fs::write(dir.path().join("selfie.png"), b"png").unwrap();

        let out = send(
            &tool,
            dir.path(),
            serde_json::json!({"text": "hi", "channel": "fake:12"}),
        )
        .await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(rx.recv().await.unwrap().chat_id, "12");

        let out = send(
            &tool,
            dir.path(),
            serde_json::json!({"text": "hi", "chat": "99"}),
        )
        .await;
        assert!(out.is_error);
        assert!(
            out.content.contains("Known: fake:12, fake:15"),
            "{}",
            out.content
        );

        let out = send(
            &tool,
            dir.path(),
            serde_json::json!({"text": "the selfie, attached"}),
        )
        .await;
        assert!(out.is_error);
        assert!(out.content.contains("media is empty"), "{}", out.content);

        let out = send(
            &tool,
            dir.path(),
            serde_json::json!({"text": "I attached the bracket to the wall", "no_attachment": true}),
        )
        .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(rx.recv().await.unwrap().media.is_empty());

        let out = send(
            &tool,
            dir.path(),
            serde_json::json!({"text": "attached", "media": ["nope.png"]}),
        )
        .await;
        assert!(out.is_error);
        assert!(out.content.contains("no file at"), "{}", out.content);

        let out = send(
            &tool,
            dir.path(),
            serde_json::json!({"text": "the selfie, attached", "media": ["selfie.png"]}),
        )
        .await;
        assert!(!out.is_error, "{}", out.content);
        let sent = rx.recv().await.unwrap();
        assert_eq!(sent.media, vec![dir.path().join("selfie.png")]);
    }

    #[tokio::test]
    async fn a_reply_takes_its_own_chats_status_line_down_and_no_others() {
        let dir = tempfile::tempdir().unwrap();
        let fake = crate::channel::FakeChannel::new("fake");
        let channels = std::collections::HashMap::from([(
            "fake".to_string(),
            fake.clone() as Arc<dyn crate::channel::Channel>,
        )]);
        let board = crate::status::StatusBoard::new(channels, true, std::time::Duration::ZERO);
        let _claim = board.begin("fake:12", "fake", "12").await.expect("a line");

        // A scheduled turn homed on that chat is not the turn the
        // person is watching: its message leaves the line alone.
        let (nightly, _out) = tool_on(dir.path(), "cron:nightly", board.clone());
        let out = send(
            &nightly,
            dir.path(),
            serde_json::json!({"text": "reminder"}),
        )
        .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(board.is_up("fake:12"), "{:?}", fake.seen());

        // The chat's own turn answering: that is what it waited for.
        let (own, _out) = tool_on(dir.path(), "fake:12", board.clone());
        let out = send(&own, dir.path(), serde_json::json!({"text": "here you go"})).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(!board.is_up("fake:12"));
    }
}
