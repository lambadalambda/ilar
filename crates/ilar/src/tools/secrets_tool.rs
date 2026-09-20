//! The `secrets` tool: what is stored, by name and purpose, and which
//! tool may already use it. Never a value.
//!
//! Always registered; shown only while a store file exists. The file,
//! not its contents: what it holds changes while a session runs, and an
//! empty store is still a store. So a machine that has never stored a
//! secret pays for no description, and the first `ilar secret set` it
//! ever runs reaches the session that prompted it on the next turn,
//! rather than after a restart. A call that arrives while it is hidden
//! is still answered — see [`super::Tool::is_published`].

use super::{Tool, ToolConcurrency, ToolContext, ToolFuture, ToolOutput, WorkspaceAccess};

pub struct SecretsTool {
    store: crate::secrets::SecretStore,
}

impl SecretsTool {
    pub fn new(store: crate::secrets::SecretStore) -> Self {
        Self { store }
    }
}

impl Tool for SecretsTool {
    fn name(&self) -> &'static str {
        "secrets"
    }

    fn is_published(&self) -> bool {
        self.store.exists()
    }

    fn description(&self) -> &'static str {
        "List the secrets the user has stored for you: names, what each is \
         for, and which tools may use it without asking. You never see a \
         value. To use one, name it in the `secrets` argument of bash or \
         service; it arrives as an environment variable of that command, \
         after the user allows it."
    }

    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Concurrent
    }

    fn workspace_access(&self) -> WorkspaceAccess {
        WorkspaceAccess::None
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    fn run(&self, _input: serde_json::Value, ctx: ToolContext) -> ToolFuture {
        Box::pin(async move {
            let Some(secrets) = ctx.secrets.as_ref() else {
                return ToolOutput::error("secrets: no store is attached to this session");
            };
            // A sealed store has no names to list either, so this is a
            // use like any other: the master password is asked for here
            // rather than at the start of every session.
            secrets
                .unlock_if_locked(&crate::secrets::Request {
                    tool: "secrets",
                    names: &[],
                    detail: "listing what you have stored",
                    session_id: &ctx.session_id,
                    tool_call_id: ctx.call_id.as_deref(),
                    cancel: &ctx.cancel,
                })
                .await;
            match secrets.listing() {
                Ok(text) => ToolOutput::text(text),
                Err(error) => ToolOutput::error(format!("secrets: {error:#}")),
            }
        })
    }
}
