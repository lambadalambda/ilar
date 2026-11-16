#![cfg(target_os = "macos")]

use ilar::tools::{ToolContext, ToolOutput, file_worker};
use serde_json::{Value, json};
use std::path::Path;

async fn call(tool: &str, input: Value, ctx: &ToolContext) -> ToolOutput {
    file_worker::run_with_program(
        tool,
        input,
        ctx,
        Path::new(env!("CARGO_BIN_EXE_ilar-file-worker")),
    )
    .await
}

#[tokio::test]
async fn workspace_read_write_edit_and_search_keep_their_behavior() {
    let dir = tempfile::tempdir().unwrap();
    let mut ctx = ToolContext::root(dir.path().to_path_buf());
    ctx.sandbox.enabled = true;
    let written = call(
        "write",
        json!({"path":"note.txt", "content":"hello\n"}),
        &ctx,
    )
    .await;
    assert!(!written.is_error, "{}", written.content);
    let read = call("read", json!({"path":"note.txt"}), &ctx).await;
    assert!(read.content.contains("1→hello"), "{}", read.content);
    let edited = call(
        "edit",
        json!({"path":"note.txt", "old_string":"hello", "new_string":"world"}),
        &ctx,
    )
    .await;
    assert!(!edited.is_error, "{}", edited.content);
    let grep = call("grep", json!({"pattern":"world"}), &ctx).await;
    assert!(
        grep.content.contains("note.txt:1:world"),
        "{}",
        grep.content
    );
    let glob = call("glob", json!({"pattern":"*.txt"}), &ctx).await;
    assert!(glob.content.contains("note.txt"), "{}", glob.content);
    std::fs::write(dir.path().join("note.txt"), "changed").unwrap();
    let stale = call(
        "edit",
        json!({"path":"note.txt", "old_string":"changed", "new_string":"no"}),
        &ctx,
    )
    .await;
    assert!(
        stale.is_error && stale.content.contains("changed since"),
        "{}",
        stale.content
    );
}

#[tokio::test]
async fn outside_files_and_symlink_targets_are_denied_until_granted() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("private.txt");
    std::fs::write(&secret, "private-marker").unwrap();
    std::os::unix::fs::symlink(&secret, workspace.path().join("alias.txt")).unwrap();
    let mut ctx = ToolContext::root(workspace.path().to_path_buf());
    ctx.sandbox.enabled = true;
    for path in [secret.to_str().unwrap(), "alias.txt"] {
        let read = call("read", json!({"path":path}), &ctx).await;
        assert!(read.is_error, "{}", read.content);
        assert!(!read.content.contains("private-marker"));
    }
    let write = call("write", json!({"path":secret, "content":"damaged"}), &ctx).await;
    assert!(write.is_error, "{}", write.content);
    assert_eq!(std::fs::read_to_string(&secret).unwrap(), "private-marker");
    let grep = call(
        "grep",
        json!({"pattern":"private-marker", "path":outside.path()}),
        &ctx,
    )
    .await;
    assert!(!grep.content.contains("private-marker"), "{}", grep.content);
    let glob = call(
        "glob",
        json!({"pattern":format!("{}/*.txt", outside.path().display())}),
        &ctx,
    )
    .await;
    assert!(!glob.content.contains("private.txt"), "{}", glob.content);
    ctx.sandbox.read_paths.push(outside.path().to_path_buf());
    let read = call("read", json!({"path":"alias.txt"}), &ctx).await;
    assert!(
        !read.is_error && read.content.contains("private-marker"),
        "{}",
        read.content
    );
    let write = call(
        "write",
        json!({"path":secret, "content":"still denied"}),
        &ctx,
    )
    .await;
    assert!(write.is_error, "{}", write.content);
    ctx.sandbox.write_paths.push(outside.path().to_path_buf());
    let edit = call(
        "edit",
        json!({"path":secret, "old_string":"private-marker", "new_string":"approved"}),
        &ctx,
    )
    .await;
    assert!(!edit.is_error, "{}", edit.content);
    assert_eq!(std::fs::read_to_string(secret).unwrap(), "approved");
}

#[tokio::test]
async fn denied_file_bypass_shows_the_operation_and_does_not_write() {
    use ilar::question::{QuestionResponse, question_channel};
    let workspace = tempfile::tempdir().unwrap();
    let mut ctx = ToolContext::root(workspace.path().to_path_buf());
    ctx.sandbox.enabled = true;
    let (sender, mut receiver) = question_channel(1);
    ctx.sandbox_questions = Some(sender);
    let deny = async {
        let prompt = receiver.recv().await.unwrap();
        let detail = prompt.request.questions[0].description.as_deref().unwrap();
        assert!(
            detail.contains("target.txt") && detail.contains("proposed contents"),
            "{detail}"
        );
        prompt.reply.send(QuestionResponse::Cancelled).unwrap();
    };
    let (output, ()) = tokio::join!(
        call(
            "write",
            json!({"path":"target.txt", "content":"proposed contents", "sandbox":{"unsandboxed":true,"reason":"Test bypass"}}),
            &ctx
        ),
        deny
    );
    assert!(output.is_error);
    assert!(!workspace.path().join("target.txt").exists());
}

#[tokio::test]
async fn exact_file_write_request_is_refused_before_approval() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let path = outside.path().join("file");
    std::fs::write(&path, "keep").unwrap();
    let mut ctx = ToolContext::root(workspace.path().to_path_buf());
    ctx.sandbox.enabled = true;
    let output = call("write", json!({"path":path,"content":"no","sandbox":{"write_paths":[path],"reason":"Replace file"}}), &ctx).await;
    assert!(
        output.is_error && output.content.contains("directory grant"),
        "{}",
        output.content
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), "keep");
    ctx.sandbox.write_paths.push(outside.path().to_path_buf());
    let output = call(
        "write",
        json!({"path":outside.path().join("new"),"content":"no"}),
        &ctx,
    )
    .await;
    assert!(output.is_error, "write-only directory: {}", output.content);
}

#[tokio::test]
async fn file_approvals_share_once_and_session_rules_with_shell_tools() {
    use ilar::question::{QuestionAnswer, QuestionResponse, question_channel};
    use ilar::tools::{Tool, bash::BashTool};
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let path = outside.path().join("reference");
    std::fs::write(&path, "approved-text").unwrap();
    for (scope, retained) in [("once", false), ("session", true)] {
        let mut ctx = ToolContext::root(workspace.path().to_path_buf());
        ctx.sandbox.enabled = true;
        ctx.session_id = scope.into();
        let (sender, mut receiver) = question_channel(1);
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
                        option_id: Some(scope.into()),
                        other: None,
                    }],
                })
                .unwrap();
        };
        let (read, ()) = tokio::join!(
            call(
                "read",
                json!({"path":path,"sandbox":{"read_paths":[outside.path()],"reason":"Read reference"}}),
                &ctx
            ),
            approve
        );
        assert!(!read.is_error, "{}", read.content);
        ctx.sandbox_questions = None;
        let later = call("read", json!({"path":path}), &ctx).await;
        assert_eq!(!later.is_error, retained, "{}", later.content);
        let shell = BashTool
            .run(json!({"command":format!("cat '{}'", path.display())}), ctx)
            .await;
        assert_eq!(!shell.is_error, retained, "{}", shell.content);
    }
}
