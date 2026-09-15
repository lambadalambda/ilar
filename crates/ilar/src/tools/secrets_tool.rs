//! The `secrets` tool: what is stored, by name and purpose, and which
//! tool may already use it. Never a value. Installed wherever a store
//! file exists — what it holds changes while a session runs, so the
//! file, not its contents, decides — and absent on a machine that has
//! never stored a secret.

use super::{Tool, ToolConcurrency, ToolContext, ToolFuture, ToolOutput, WorkspaceAccess};

pub struct SecretsTool;

impl Tool for SecretsTool {
    fn name(&self) -> &'static str {
        "secrets"
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
            match secrets.listing() {
                Ok(text) => ToolOutput::text(text),
                Err(error) => ToolOutput::error(format!("secrets: {error:#}")),
            }
        })
    }
}
