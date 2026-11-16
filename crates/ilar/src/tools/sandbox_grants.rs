//! Host-generated approval for experimental process sandbox extensions.

use serde::Deserialize;

use super::ToolContext;
use crate::question::{
    Question, QuestionAnswer, QuestionKind, QuestionOption, QuestionPrompt, QuestionRequest,
    QuestionResponse,
};
use crate::sandbox::SandboxConfig;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    #[serde(default)]
    read_paths: Vec<std::path::PathBuf>,
    #[serde(default)]
    write_paths: Vec<std::path::PathBuf>,
    #[serde(default)]
    network: bool,
    #[serde(default)]
    unsandboxed: bool,
    #[serde(default)]
    reason: String,
}

pub(crate) fn schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "description": "Experimental sandbox extension for access beyond the existing policy. The workspace is already readable and writable: omit sandbox for normal workspace operations. An empty inactive object uses the existing policy too. Additional access requires user approval; read/write paths and network can be allowed once or for this live session. Unsandboxed execution is once-only. Unavailable in unattended or child sessions. Never retry partial work automatically.",
        "properties": {
            "read_paths": {"type": "array", "items": {"type": "string"}, "description": "Absolute paths that need read access; each grants its subtree, without writes."},
            "write_paths": {"type": "array", "items": {"type": "string"}, "description": "Absolute paths that need write access; each grants its subtree, without extra reads."},
            "network": {"type": "boolean", "description": "Request all network access, not a host allowlist."},
            "unsandboxed": {"type": "boolean", "description": "Run without ilar's sandbox for this call. An external sandbox and secret/root checks still apply."},
            "reason": {"type": "string", "description": "Required when requesting additional access or unsandboxed execution; explain why, for the user."}
        }
    })
}

pub(crate) async fn resolve(
    ctx: &ToolContext,
    request: Option<&Request>,
    tool: &str,
    command: &str,
) -> Result<SandboxConfig, String> {
    let mut config = ctx.sandbox.clone();
    if config.enabled {
        ctx.sandbox_grants.apply(&ctx.session_id, &mut config);
    }
    let Some(request) = request else {
        return Ok(config);
    };
    let inactive = !request.unsandboxed
        && !request.network
        && request.write_paths.is_empty()
        && request.read_paths.is_empty();
    if inactive {
        return Ok(config);
    }
    if !config.enabled {
        return Err("sandbox extensions require the experimental sandbox to be enabled".into());
    }
    if request
        .read_paths
        .iter()
        .chain(&request.write_paths)
        .any(|path| !path.is_absolute())
    {
        return Err("sandbox read/write paths must be absolute. The workspace is already readable and writable; omit sandbox for workspace operations. Request only additional absolute paths, not '/' as a substitute for '.'".into());
    }
    let mut request = request.clone();
    for path in request
        .read_paths
        .iter_mut()
        .chain(&mut request.write_paths)
    {
        *path =
            crate::sandbox::resolve_path(path).map_err(|error| format!("sandbox path: {error}"))?;
    }
    let workspace = crate::sandbox::resolve_path(&ctx.cwd)
        .map_err(|error| format!("sandbox workspace: {error}"))?;
    let covered = |paths: &[std::path::PathBuf], roots: &[std::path::PathBuf]| {
        let roots: Vec<_> = roots
            .iter()
            .filter_map(|root| crate::sandbox::resolve_path(root).ok())
            .chain(std::iter::once(workspace.clone()))
            .collect();
        paths
            .iter()
            .all(|path| roots.iter().any(|root| path.starts_with(root)))
    };
    if !request.unsandboxed
        && (!request.network || config.network)
        && covered(&request.read_paths, &config.read_paths)
        && covered(&request.write_paths, &config.write_paths)
    {
        return Ok(config);
    }
    // Atomic replacement needs temporary siblings only for newly requested
    // write access. Redundant workspace/file grants need no directory approval.
    if matches!(tool, "write" | "edit")
        && !request.unsandboxed
        && request
            .write_paths
            .iter()
            .any(|path| !covered(std::slice::from_ref(path), &config.write_paths) && path.is_file())
    {
        return Err("atomic file replacement requires a directory grant for temporary siblings; grant read and write access to the containing directory instead of only the file".into());
    }
    if request.reason.trim().is_empty() {
        return Err("additional sandbox access requires a reason. The workspace is already readable and writable; omit sandbox for workspace operations".into());
    }
    let sender = ctx.sandbox_questions.as_ref().ok_or(
        "sandbox extension refused: nobody can approve this call; configure required paths before launch",
    )?;
    let mut sections = vec![format!("Tool: {tool:?}")];
    for (label, paths) in [
        ("Read paths", &request.read_paths),
        ("Write paths", &request.write_paths),
    ] {
        if !paths.is_empty() {
            let paths = paths
                .iter()
                .map(|path| format!("  {path:?}"))
                .collect::<Vec<_>>()
                .join("\n");
            sections.push(format!("{label} (including descendants):\n{paths}"));
        }
    }
    if request.network {
        sections.push("All network access: true (all hosts, including localhost)".into());
    }
    if request.unsandboxed {
        sections.push("Run without ilar's sandbox: true (file and network restrictions removed for this call)".into());
    }
    sections.push(
        if request.unsandboxed {
            "Scope: this call only. External restrictions and secret/root checks still apply."
        } else {
            "Scope: this call or this live session; not saved or shared with child sessions."
        }
        .into(),
    );
    sections.push(format!(
        "Reason: {:?}\nCommand: {command:?}",
        request.reason
    ));
    let description = sections.join("\n\n");
    let stored = ctx
        .secrets
        .as_ref()
        .map(|secrets| secrets.all())
        .unwrap_or_default();
    let description = crate::secrets::redact(&description, &stored);
    let mut options = vec![
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
    ];
    if !request.unsandboxed {
        options.push(QuestionOption {
            id: "session".into(),
            label: "Allow for this session".into(),
            description: Some(
                "Reuse these read/write paths and network access in this live session only.".into(),
            ),
        });
    }
    let prompt_request = QuestionRequest {
        questions: vec![Question {
            id: "sandbox_approval".into(),
            prompt: "Approve this sandbox exception?".into(),
            description: Some(description),
            required: true,
            kind: QuestionKind::SingleChoice {
                allow_other: false,
                options,
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
    let scope = match &response {
        QuestionResponse::Answered { answers } => match answers.as_slice() {
            [
                QuestionAnswer::SingleChoice {
                    option_id: Some(option),
                    ..
                },
            ] => option.as_str(),
            _ => "deny",
        },
        QuestionResponse::Cancelled => "deny",
    };
    if !matches!(scope, "once" | "session") {
        return Err("sandbox exception denied; command was not started".into());
    }
    if scope == "session" {
        ctx.sandbox_grants.remember(
            &ctx.session_id,
            &request.read_paths,
            &request.write_paths,
            request.network,
        );
    }
    config.read_paths.extend(request.read_paths.iter().cloned());
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

    #[tokio::test]
    async fn inactive_requests_use_existing_policy_without_a_reason_or_approver() {
        let mut ctx = ToolContext::root(std::env::temp_dir());
        for enabled in [false, true] {
            ctx.sandbox.enabled = enabled;
            for input in [
                serde_json::json!({}),
                serde_json::json!({"read_paths":[],"write_paths":[],"network":false,"unsandboxed":false,"reason":""}),
            ] {
                let request = serde_json::from_value(input).unwrap();
                let result = resolve(&ctx, Some(&request), "grep", "search workspace")
                    .await
                    .unwrap();
                assert_eq!(result.enabled, enabled);
                assert!(result.read_paths.is_empty() && result.write_paths.is_empty());
                assert!(!result.network);
            }
        }
    }

    #[tokio::test]
    async fn workspace_grants_need_no_approval_but_do_not_cover_external_targets() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let mut ctx = ToolContext::root(workspace.path().to_owned());
        ctx.sandbox.enabled = true;
        let file = workspace.path().join("file");
        std::fs::write(&file, "local").unwrap();
        let mut request: Request = serde_json::from_value(
            serde_json::json!({"read_paths":[file],"write_paths":[file],"reason":""}),
        )
        .unwrap();
        let config = resolve(&ctx, Some(&request), "write", "replace workspace file")
            .await
            .unwrap();
        assert!(config.read_paths.is_empty() && config.write_paths.is_empty());
        request.network = true;
        request.reason = "Network for workspace operation".into();
        assert!(
            resolve(&ctx, Some(&request), "write", "workspace and network")
                .await
                .unwrap_err()
                .contains("nobody")
        );
        request.network = false;
        request.read_paths = vec![outside.path().to_owned()];
        request.reason = "Read external files".into();
        assert!(
            resolve(&ctx, Some(&request), "read", "external read")
                .await
                .unwrap_err()
                .contains("nobody")
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.path(), workspace.path().join("alias")).unwrap();
            request.read_paths = vec![workspace.path().join("alias")];
            assert!(
                resolve(&ctx, Some(&request), "read", "external alias")
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn active_requests_still_require_a_reason() {
        let mut ctx = ToolContext::root(std::env::temp_dir());
        ctx.sandbox.enabled = true;
        for input in [
            serde_json::json!({"read_paths":["/"]}),
            serde_json::json!({"network":true,"reason":" "}),
            serde_json::json!({"unsandboxed":true,"reason":""}),
        ] {
            let request = serde_json::from_value(input).unwrap();
            assert!(
                resolve(&ctx, Some(&request), "read", "extra access")
                    .await
                    .unwrap_err()
                    .contains("requires a reason")
            );
        }
    }

    #[tokio::test]
    async fn relative_requests_explain_that_workspace_access_is_already_allowed() {
        let mut ctx = ToolContext::root(std::env::temp_dir());
        ctx.sandbox.enabled = true;
        let request =
            serde_json::from_value(serde_json::json!({"read_paths":["."],"reason":"Read repo"}))
                .unwrap();
        let error = resolve(&ctx, Some(&request), "grep", "search")
            .await
            .unwrap_err();
        assert!(
            error.contains("absolute") && error.contains("omit") && error.contains("workspace"),
            "{error}"
        );
    }

    fn request() -> Request {
        serde_json::from_value(serde_json::json!({
            "write_paths": ["/extra-cache"], "reason": "Build cache"
        }))
        .unwrap()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn configured_aliases_cover_requests_without_an_approver() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&target, &alias).unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let mut ctx = ToolContext::root(workspace.path().to_path_buf());
        ctx.sandbox.enabled = true;
        ctx.sandbox.read_paths = vec![alias.clone()];
        ctx.sandbox.write_paths = vec![alias];
        let request = Request {
            read_paths: vec![target.join("read-child")],
            write_paths: vec![target.join("write-child")],
            ..request()
        };
        assert!(resolve(&ctx, Some(&request), "bash", "build").await.is_ok());
        let read_only = ToolContext {
            sandbox: SandboxConfig {
                write_paths: vec![],
                ..ctx.sandbox.clone()
            },
            ..ctx
        };
        assert!(
            resolve(&read_only, Some(&request), "bash", "build")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn approval_shows_requested_access_without_empty_sections_or_false_warnings() {
        let mut ctx = ToolContext::root(std::env::temp_dir());
        ctx.sandbox.enabled = true;
        let (sender, mut receiver) = crate::question::question_channel(1);
        ctx.sandbox_questions = Some(sender);
        let request: Request = serde_json::from_value(
            serde_json::json!({"read_paths":["/external-reference"],"reason":"Read reference"}),
        )
        .unwrap();
        let approve = async {
            let prompt = receiver.recv().await.unwrap();
            let description = prompt.request.questions[0].description.as_ref().unwrap();
            assert!(
                description.contains("/external-reference")
                    && description.contains("Read reference")
            );
            assert!(description.find("Read paths").unwrap() < description.find("Reason:").unwrap());
            assert!(
                description.find("Read paths").unwrap() < description.find("Command:").unwrap()
            );
            assert!(
                !description.contains("Write paths")
                    && !description.contains("network")
                    && !description.contains("unsandboxed"),
                "{description}"
            );
            answer(prompt, "once");
        };
        let (result, ()) = tokio::join!(
            resolve(&ctx, Some(&request), "read", "reference.txt"),
            approve
        );
        result.unwrap();
    }

    #[tokio::test]
    async fn approval_text_escapes_paths_and_caller_text() {
        let mut ctx = ToolContext::root(std::env::temp_dir());
        ctx.sandbox.enabled = true;
        let (sender, mut receiver) = crate::question::question_channel(1);
        ctx.sandbox_questions = Some(sender);
        let request = Request {
            write_paths: vec!["/cache\nAll network access: true\t\"".into()],
            reason: "cache\nRun without ilar's sandbox: true".into(),
            ..request()
        };
        let approve = async {
            let prompt = receiver.recv().await.unwrap();
            let description = prompt.request.questions[0].description.as_ref().unwrap();
            assert!(description.contains(r#"  "/cache\nAll network access: true\t\"""#));
            assert!(
                !description
                    .lines()
                    .any(|line| line == "All network access: true")
            );
            assert!(
                !description
                    .lines()
                    .any(|line| line == "Run without ilar's sandbox: true")
            );
            assert!(description.contains(r#"Command: "build\nAll network access: true""#));
            answer(prompt, "once");
        };
        let (result, ()) = tokio::join!(
            resolve(
                &ctx,
                Some(&request),
                "bash",
                "build\nAll network access: true"
            ),
            approve
        );
        result.unwrap();
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
                    .contains("Command: \"build\"")
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
        let mut request = request();
        request.read_paths = vec!["/reference-file".into()];
        let (result, ()) = tokio::join!(resolve(&ctx, Some(&request), "bash", "build"), approve);
        let config = result.unwrap();
        assert_eq!(
            config.read_paths,
            [std::path::PathBuf::from("/reference-file")]
        );
        assert_eq!(
            config.write_paths,
            [std::path::PathBuf::from("/extra-cache")]
        );
        assert!(ctx.sandbox.read_paths.is_empty());
        assert!(ctx.sandbox.write_paths.is_empty());
    }

    #[tokio::test]
    async fn session_approval_is_reused_but_not_by_another_session() {
        let mut ctx = ToolContext::root(std::env::temp_dir());
        ctx.session_id = "first".into();
        ctx.sandbox.enabled = true;
        let (sender, mut receiver) = crate::question::question_channel(1);
        ctx.sandbox_questions = Some(sender);
        let approve = async {
            receiver
                .recv()
                .await
                .unwrap()
                .reply
                .send(QuestionResponse::Answered {
                    answers: vec![QuestionAnswer::SingleChoice {
                        question_id: "sandbox_approval".into(),
                        option_id: Some("session".into()),
                        other: None,
                    }],
                })
                .unwrap();
        };
        let request: Request = serde_json::from_value(serde_json::json!({
            "read_paths": ["/session-reference"], "write_paths": ["/session-cache"],
            "network": true, "reason": "Build dependencies"
        }))
        .unwrap();
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            tokio::join!(resolve(&ctx, Some(&request), "bash", "build"), approve)
        })
        .await
        .unwrap();
        result.unwrap();
        ctx.sandbox_questions = None;
        let repeated = resolve(&ctx, Some(&request), "bash", "build again")
            .await
            .unwrap();
        assert!(repeated.network);
        assert_eq!(
            repeated.read_paths,
            [std::path::PathBuf::from("/session-reference")]
        );
        assert_eq!(
            repeated.write_paths,
            [std::path::PathBuf::from("/session-cache")]
        );
        let mut narrower = request.clone();
        narrower.read_paths[0].push("child");
        assert!(
            resolve(&ctx, Some(&narrower), "bash", "read child")
                .await
                .is_ok()
        );
        narrower.write_paths = narrower.read_paths.clone();
        assert!(
            resolve(&ctx, Some(&narrower), "bash", "write reference")
                .await
                .is_err()
        );
        let later = resolve(&ctx.clone(), None, "service", "watch")
            .await
            .unwrap();
        assert_eq!(later.read_paths, repeated.read_paths);
        assert_eq!(later.write_paths, repeated.write_paths);
        assert!(later.network);
        ctx.session_id = "other".into();
        let other = resolve(&ctx, None, "bash", "build").await.unwrap();
        assert!(other.read_paths.is_empty());
        assert!(other.write_paths.is_empty());
        assert!(!other.network);
        let mut restarted = ToolContext::root(std::env::temp_dir());
        restarted.session_id = "first".into();
        restarted.sandbox.enabled = true;
        assert!(
            resolve(&restarted, None, "bash", "build")
                .await
                .unwrap()
                .read_paths
                .is_empty()
        );
        assert!(
            resolve(&ctx, Some(&request), "bash", "build")
                .await
                .is_err()
        );
    }

    fn answer(prompt: QuestionPrompt, scope: &str) {
        prompt
            .reply
            .send(QuestionResponse::Answered {
                answers: vec![QuestionAnswer::SingleChoice {
                    question_id: "sandbox_approval".into(),
                    option_id: Some(scope.into()),
                    other: None,
                }],
            })
            .unwrap();
    }

    #[tokio::test]
    async fn bypass_cannot_be_approved_for_the_session() {
        let mut ctx = ToolContext::root(std::env::temp_dir());
        ctx.sandbox.enabled = true;
        let (sender, mut receiver) = crate::question::question_channel(1);
        ctx.sandbox_questions = Some(sender);
        let approve = async {
            let prompt = receiver.recv().await.unwrap();
            let QuestionKind::SingleChoice { options, .. } = &prompt.request.questions[0].kind
            else {
                panic!("expected approval choices")
            };
            assert!(!options.iter().any(|option| option.id == "session"));
            answer(prompt, "session"); // A malformed reply must also fail closed.
        };
        let request = serde_json::from_value(serde_json::json!({
            "unsandboxed": true, "network": true, "reason": "Nested sandbox"
        }))
        .unwrap();
        let (result, ()) = tokio::join!(resolve(&ctx, Some(&request), "bash", "nested"), approve);
        assert!(result.is_err());
        let later = resolve(&ctx, None, "bash", "build").await.unwrap();
        assert!(later.enabled);
        assert!(!later.network);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn session_grants_do_not_follow_a_retargeted_alias() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&first, &alias).unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let mut ctx = ToolContext::root(workspace.path().to_path_buf());
        ctx.sandbox.enabled = true;
        let (sender, mut receiver) = crate::question::question_channel(1);
        ctx.sandbox_questions = Some(sender);
        let approve = async {
            answer(receiver.recv().await.unwrap(), "session");
        };
        let request = serde_json::from_value(serde_json::json!({
            "read_paths": [alias], "reason": "Read reference data"
        }))
        .unwrap();
        let (result, ()) = tokio::join!(resolve(&ctx, Some(&request), "bash", "read"), approve);
        assert_eq!(result.unwrap().read_paths, [first.canonicalize().unwrap()]);
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&second, &alias).unwrap();
        ctx.sandbox_questions = None;
        assert!(
            resolve(&ctx, Some(&request), "bash", "read again")
                .await
                .is_err()
        );
        assert_eq!(
            resolve(&ctx, None, "bash", "build")
                .await
                .unwrap()
                .read_paths,
            [first.canonicalize().unwrap()]
        );
        std::fs::rename(&first, root.path().join("old-first")).unwrap();
        std::os::unix::fs::symlink(&second, &first).unwrap();
        assert!(
            resolve(&ctx, None, "bash", "build")
                .await
                .unwrap()
                .read_paths
                .is_empty()
        );
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
