//! Host-generated, once-only approval for experimental process sandbox extensions.

use serde::Deserialize;

use super::ToolContext;
use crate::question::{
    Question, QuestionAnswer, QuestionKind, QuestionOption, QuestionPrompt, QuestionRequest,
    QuestionResponse,
};
use crate::sandbox::SandboxConfig;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    #[serde(default)]
    write_paths: Vec<std::path::PathBuf>,
    #[serde(default)]
    network: bool,
    #[serde(default)]
    unsandboxed: bool,
    reason: String,
}

pub(crate) fn schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "description": "Experimental sandbox extension for this call only. Requires user approval before execution; unavailable in unattended or child sessions. Never retry partial work automatically. Omit for normal execution.",
        "properties": {
            "write_paths": {"type": "array", "items": {"type": "string"}, "description": "Absolute paths that need write access; each grants its subtree."},
            "network": {"type": "boolean", "description": "Request all network access, not a host allowlist."},
            "unsandboxed": {"type": "boolean", "description": "Run without ilar's sandbox for this call. An external sandbox and secret/root checks still apply."},
            "reason": {"type": "string", "description": "Why this call needs the extra access; shown to the user."}
        },
        "required": ["reason"]
    })
}

pub(crate) async fn resolve(
    ctx: &ToolContext,
    request: Option<&Request>,
    tool: &str,
    command: &str,
) -> Result<SandboxConfig, String> {
    let mut config = ctx.sandbox.clone();
    let Some(request) = request else {
        return Ok(config);
    };
    if !config.enabled {
        return Err("sandbox extensions require the experimental sandbox to be enabled".into());
    }
    if request.reason.trim().is_empty() {
        return Err("sandbox extension requires a reason".into());
    }
    if request.write_paths.iter().any(|path| !path.is_absolute()) {
        return Err("sandbox write paths must be absolute".into());
    }
    if !request.unsandboxed && !request.network && request.write_paths.is_empty() {
        return Err("sandbox extension requests no additional access".into());
    }
    let sender = ctx.sandbox_questions.as_ref().ok_or(
        "sandbox extension refused: nobody can approve this call; configure required paths before launch",
    )?;
    let paths = request
        .write_paths
        .iter()
        .map(|path| format!("  {}", path.display()))
        .collect::<Vec<_>>()
        .join("\n");
    let description = format!(
        "Tool: {tool}\nCommand: {command}\nReason: {}\nWrite paths (including descendants):\n{paths}\nAll network access: {}\nRun without ilar's sandbox: {}\nThis approval is for one call. It does not remove an external sandbox or secret/root checks.",
        request.reason, request.network, request.unsandboxed
    );
    let stored = ctx
        .secrets
        .as_ref()
        .map(|secrets| secrets.all())
        .unwrap_or_default();
    let description = crate::secrets::redact(&description, &stored);
    let prompt_request = QuestionRequest {
        questions: vec![Question {
            id: "sandbox_approval".into(),
            prompt: "Approve this sandbox exception?".into(),
            description: Some(description),
            required: true,
            kind: QuestionKind::SingleChoice {
                allow_other: false,
                options: vec![
                    QuestionOption {
                        id: "deny".into(),
                        label: "Deny".into(),
                        description: None,
                    },
                    QuestionOption {
                        id: "once".into(),
                        label: "Allow this call only".into(),
                        description: None,
                    },
                ],
            },
        }],
    };
    let (reply, receive) = tokio::sync::oneshot::channel();
    let prompt = QuestionPrompt {
        session_id: ctx.session_id.clone(),
        tool_call_id: ctx.call_id.clone().unwrap_or_default(),
        request: prompt_request.clone(),
        reply,
    };
    let response = tokio::select! {
        biased;
        _ = ctx.cancel.cancelled() => return Err("sandbox approval cancelled".into()),
        response = async {
            sender.send(prompt).await.map_err(|_| "sandbox approval channel closed")?;
            receive.await.map_err(|_| "sandbox approval was not answered")
        } => response?,
    };
    response
        .validate(&prompt_request)
        .map_err(|error| format!("invalid sandbox approval: {error}"))?;
    let approved = matches!(response, QuestionResponse::Answered { ref answers }
        if matches!(answers.as_slice(), [QuestionAnswer::SingleChoice { question_id, option_id: Some(option), other: None }]
            if question_id == "sandbox_approval" && option == "once"));
    if !approved {
        return Err("sandbox exception denied; command was not started".into());
    }
    config
        .write_paths
        .extend(request.write_paths.iter().cloned());
    config.network |= request.network;
    if request.unsandboxed {
        config.enabled = false;
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> Request {
        serde_json::from_value(serde_json::json!({
            "write_paths": ["/extra-cache"], "reason": "Build cache"
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn extensions_require_an_approver() {
        let mut ctx = ToolContext::root(std::env::temp_dir());
        ctx.sandbox.enabled = true;
        assert!(
            resolve(&ctx, Some(&request()), "bash", "build")
                .await
                .unwrap_err()
                .contains("nobody")
        );
    }

    #[tokio::test]
    async fn approval_changes_only_this_call() {
        let mut ctx = ToolContext::root(std::env::temp_dir());
        ctx.sandbox.enabled = true;
        let (sender, mut receiver) = crate::question::question_channel(1);
        ctx.sandbox_questions = Some(sender);
        let approve = async {
            let prompt = receiver.recv().await.unwrap();
            assert!(
                prompt.request.questions[0]
                    .description
                    .as_ref()
                    .unwrap()
                    .contains("Command: build")
            );
            prompt
                .reply
                .send(QuestionResponse::Answered {
                    answers: vec![QuestionAnswer::SingleChoice {
                        question_id: "sandbox_approval".into(),
                        option_id: Some("once".into()),
                        other: None,
                    }],
                })
                .unwrap();
        };
        let request = request();
        let (result, ()) = tokio::join!(resolve(&ctx, Some(&request), "bash", "build"), approve);
        assert_eq!(
            result.unwrap().write_paths,
            [std::path::PathBuf::from("/extra-cache")]
        );
        assert!(ctx.sandbox.write_paths.is_empty());
    }

    #[tokio::test]
    async fn cancelled_approval_does_not_disable_sandbox() {
        let mut ctx = ToolContext::root(std::env::temp_dir());
        ctx.sandbox.enabled = true;
        let (sender, mut receiver) = crate::question::question_channel(1);
        ctx.sandbox_questions = Some(sender);
        let deny = async {
            receiver
                .recv()
                .await
                .unwrap()
                .reply
                .send(QuestionResponse::Cancelled)
                .unwrap();
        };
        let request = serde_json::from_value(
            serde_json::json!({"unsandboxed": true, "reason": "Nested sandbox"}),
        )
        .unwrap();
        let (result, ()) = tokio::join!(resolve(&ctx, Some(&request), "bash", "nested"), deny);
        assert!(result.is_err());
        assert!(ctx.sandbox.enabled);
    }
}
