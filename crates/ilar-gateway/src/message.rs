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
             nothing this turn. Other known chats can be named with channel and chat.{}",
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
            let channel = input.channel.unwrap_or(home_channel);
            let chat_id = input.chat.unwrap_or(home_chat);
            let key = session_key(&channel, &chat_id);
            // Only chats that have written are addressable: the model
            // does not get to open conversations with strangers.
            if routes.snapshot().session_for(&key).is_none() {
                return ToolOutput::error(format!(
                    "message: no chat {key}; only chats that have written to you can be messaged"
                ));
            }
            if input.text.trim().is_empty() && input.media.is_empty() {
                return ToolOutput::error("message: nothing to send");
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
