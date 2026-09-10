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
use crate::routes::RouteStore;

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
}

pub struct MessageTool {
    outbound: mpsc::Sender<Outbound>,
    home_channel: String,
    home_chat: String,
    routes: Arc<RouteStore>,
    /// Where a relative media path is looked for.
    workspace: PathBuf,
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
    pub fn new(
        outbound: mpsc::Sender<Outbound>,
        home_channel: &str,
        home_chat: &str,
        routes: Arc<RouteStore>,
        workspace: PathBuf,
        constraints: &str,
    ) -> (Arc<Self>, Arc<AtomicUsize>) {
        let sent = Arc::new(AtomicUsize::new(0));
        let description = format!(
            "Send a message to the person you are talking to, on {home_channel}. Call it for \
             every reply you want them to see; your final text is delivered only if you sent \
             nothing this turn. Another known chat is named with channel (the channel's name, \
             {home_channel}) and chat (its id). Files travel in media, by path: the text alone \
             attaches nothing, and a text that speaks of an attachment without one is refused.{}",
            if constraints.is_empty() {
                String::new()
            } else {
                format!(" Delivery constraints: {constraints}")
            }
        );
        let tool = Arc::new(Self {
            outbound,
            home_channel: home_channel.to_string(),
            home_chat: home_chat.to_string(),
            routes,
            workspace,
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
                "media": {"type": "array", "items": {"type": "string"}, "description": "Files to attach, by path (relative to the workspace or absolute); images arrive as pictures"}
            },
            "required": ["text"]
        })
    }

    fn run(&self, input: serde_json::Value, _ctx: ToolContext) -> ToolFuture {
        // The future outlives this borrow: everything it needs is cloned.
        let outbound = self.outbound.clone();
        let home_channel = self.home_channel.clone();
        let home_chat = self.home_chat.clone();
        let routes = self.routes.clone();
        let workspace = self.workspace.clone();
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
                let known: Vec<&str> = routes.sessions.keys().map(String::as_str).collect();
                return ToolOutput::error(format!(
                    "message: no chat {key}; only chats that have written to you can be \
                     messaged. Known: {}",
                    known.join(", ")
                ));
            }
            if input.text.trim().is_empty() && input.media.is_empty() {
                return ToolOutput::error("message: nothing to send");
            }
            if input.media.is_empty() && claims_attachment(&input.text) {
                return ToolOutput::error(
                    "message: the text speaks of an attachment but media is empty; put the \
                     file's path in media, or reword the text",
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
            };
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
        let (tx, rx) = mpsc::channel(4);
        let routes = Arc::new(RouteStore::open(dir.join("routes.json")).unwrap());
        routes
            .update(|routes| {
                routes.bind("fake:12", "s1");
                routes.bind("fake:15", "s2");
            })
            .unwrap();
        let (tool, _) = MessageTool::new(tx, "fake", "12", routes, dir.to_path_buf(), "");
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
}
