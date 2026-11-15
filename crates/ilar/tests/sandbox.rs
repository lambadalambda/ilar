//! Experimental process-sandbox checks. The example-project probe is opt-in.

use ilar::tools::{Tool, ToolContext, bash::BashTool};
use serde_json::json;

#[test]
fn extension_arguments_are_not_published_by_default() {
    let definition = |registry: ilar::tools::ToolRegistry| {
        registry
            .definitions()
            .into_iter()
            .find(|tool| tool.name == "bash")
            .unwrap()
    };
    assert!(
        definition(ilar::tools::ToolRegistry::builtin()).input_schema["properties"]
            .get("sandbox")
            .is_none()
    );
    assert!(
        definition(ilar::tools::ToolRegistry::builtin().with_sandbox_extensions(true)).input_schema
            ["properties"]
            .get("sandbox")
            .is_some()
    );
}

#[tokio::test]
async fn an_unapproved_exception_does_not_start_the_command() {
    let project = tempfile::tempdir().unwrap();
    let mut ctx = ToolContext::root(project.path().to_path_buf());
    ctx.sandbox.enabled = true;
    let output = BashTool
        .run(
            json!({
                "command": "touch should-not-exist",
                "sandbox": {"unsandboxed": true, "reason": "Nested sandbox"}
            }),
            ctx,
        )
        .await;
    assert!(output.is_error, "{}", output.content);
    assert!(!project.path().join("should-not-exist").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn sandbox_bypass_does_not_retry_sudo_from_command_output() {
    use ilar::question::{QuestionAnswer, QuestionResponse};
    use std::os::unix::fs::PermissionsExt;
    let project = tempfile::tempdir().unwrap();
    let binary = project.path().join("fake-sudo");
    std::fs::write(&binary, "#!/bin/sh\nif [ \"$1\" = -S ]; then read pw; shift 3; else shift; fi\n[ \"$1\" = -- ] && shift\nexec \"$@\"\n").unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let store = ilar::secrets::SecretStore::open(&project.path().join("state"));
    store.grant_always("root", "sudo").unwrap();
    store
        .set("SUDO_PASSWORD", "test password", "test-value")
        .unwrap();
    let (asks, _ask_receiver) = ilar::secrets::ask_channel(1);
    let mut ctx = ToolContext::root(project.path().to_path_buf())
        .with_secrets(ilar::secrets::Secrets::new(store).with_prompts(asks));
    ctx.sandbox.enabled = true;
    let (questions, mut receiver) = ilar::question::question_channel(1);
    ctx.sandbox_questions = Some(questions);
    let approve = async {
        receiver
            .recv()
            .await
            .unwrap()
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
    let tool = ilar::tools::sudo::SudoTool::with_binary(binary.to_str().unwrap());
    let call = tool.run(
        json!({
            "command": "echo executed >> count; echo 'incorrect password attempt'; exit 1",
            "reason": "Test fake sudo",
            "sandbox": {"unsandboxed": true, "reason": "Test one approved launch"}
        }),
        ctx,
    );
    let (output, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(call, approve)
    })
    .await
    .expect("must not ask for a retry after command output");
    assert!(output.is_error);
    assert!(
        output.content.contains("incorrect password attempt"),
        "{}",
        output.content
    );
    assert_eq!(
        std::fs::read_to_string(project.path().join("count")).unwrap(),
        "executed\n"
    );
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::path::Path;

    fn quote(path: &Path) -> String {
        ilar::tools::sudo::shell_quote(path.to_str().unwrap())
    }

    // Keep the protected fixture separate from the granted project.
    fn fixture() -> tempfile::TempDir {
        tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap()
    }

    async fn run(
        project: &Path,
        command: String,
        grants: Vec<std::path::PathBuf>,
    ) -> ilar::tools::ToolOutput {
        let mut ctx = ToolContext::root(project.to_path_buf());
        ctx.sandbox.enabled = true;
        ctx.sandbox.write_paths = grants;
        BashTool
            .run(json!({"command": command, "timeout_ms": 120000}), ctx)
            .await
    }

    #[tokio::test]
    #[ignore = "requires a host that permits Seatbelt activation"]
    async fn unrelated_reads_and_outward_read_symlinks_are_denied() {
        let root = fixture();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let protected = root.path().join("private");
        std::fs::write(&protected, "synthetic private content").unwrap();
        std::os::unix::fs::symlink(&protected, project.join("link")).unwrap();
        std::fs::write(project.join("visible"), "workspace content").unwrap();
        let visible = run(&project, "cat visible".into(), vec![]).await;
        assert!(!visible.is_error, "{}", visible.content);
        assert!(visible.content.contains("workspace content"));
        let data_alias =
            Path::new("/System/Volumes/Data").join(protected.strip_prefix("/").unwrap());
        let mut paths = vec![protected.clone(), project.join("link")];
        if data_alias.is_file() {
            paths.push(data_alias);
        }
        for path in paths {
            let output = run(&project, format!("cat {}", quote(&path)), vec![]).await;
            assert!(output.is_error, "{}", output.content);
            assert!(!output.content.contains("synthetic private content"));
        }
        let listing = run(&project, format!("ls {}", quote(root.path())), vec![]).await;
        assert!(listing.is_error, "{}", listing.content);
    }

    #[tokio::test]
    #[ignore = "requires a host that permits Seatbelt activation"]
    async fn explicit_reads_do_not_grant_writes_or_sibling_reads() {
        let root = fixture();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let reference = root.path().join("reference");
        std::fs::write(&reference, "reference content").unwrap();
        let mut ctx = ToolContext::root(project);
        ctx.sandbox.enabled = true;
        ctx.sandbox.read_paths = vec![reference.clone()];
        let output = BashTool.run(json!({"command": format!("cat {}; printf changed >> {}", quote(&reference), quote(&reference))}), ctx.clone()).await;
        assert!(output.is_error, "{}", output.content);
        assert!(
            output.content.contains("reference content"),
            "{}",
            output.content
        );
        assert_eq!(
            std::fs::read_to_string(&reference).unwrap(),
            "reference content"
        );
        let sibling = root.path().join("sibling");
        std::fs::write(&sibling, "sibling content").unwrap();
        let output = BashTool
            .run(json!({"command": format!("cat {}", quote(&sibling))}), ctx)
            .await;
        assert!(output.is_error);
        assert!(!output.content.contains("sibling content"));
    }

    #[tokio::test]
    #[ignore = "requires a host that permits Seatbelt activation"]
    async fn read_approval_scope_is_enforced_on_later_calls() {
        use ilar::question::{QuestionAnswer, QuestionResponse};
        let root = fixture();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let reference = root.path().join("reference");
        std::fs::write(&reference, "approved reference").unwrap();
        for scope in ["once", "session"] {
            let mut ctx = ToolContext::root(project.clone());
            ctx.sandbox.enabled = true;
            let (sender, mut receiver) = ilar::question::question_channel(1);
            ctx.sandbox_questions = Some(sender);
            let approve = async {
                let prompt = receiver.recv().await.unwrap();
                assert!(
                    prompt.request.questions[0]
                        .description
                        .as_ref()
                        .unwrap()
                        .contains("Read paths")
                );
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
            };
            let command = format!("cat {}", quote(&reference));
            let (output, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                tokio::join!(
                    BashTool.run(
                        json!({"command": command, "sandbox": {
                            "read_paths": [reference], "reason": "Read the reference file"
                        }}),
                        ctx.clone()
                    ),
                    approve
                )
            })
            .await
            .expect("read approval should complete");
            assert!(!output.is_error, "{}", output.content);
            assert!(output.content.contains("approved reference"));
            let output = BashTool.run(json!({"command": command}), ctx).await;
            assert_eq!(output.is_error, scope == "once", "{}", output.content);
            assert_eq!(
                output.content.contains("approved reference"),
                scope == "session"
            );
        }
    }

    #[tokio::test]
    #[ignore = "requires a host that permits Seatbelt activation"]
    async fn denied_write_preserves_partial_work_and_does_not_follow_a_symlink() {
        let root = fixture();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let protected = root.path().join("protected");
        std::fs::write(&protected, "original").unwrap();
        std::os::unix::fs::symlink(&protected, project.join("link")).unwrap();
        let output = run(
            &project,
            "printf started > marker; printf changed > link".into(),
            vec![],
        )
        .await;
        assert!(output.is_error, "{}", output.content);
        assert_eq!(
            std::fs::read_to_string(project.join("marker")).unwrap(),
            "started"
        );
        assert_eq!(std::fs::read_to_string(&protected).unwrap(), "original");
    }

    #[tokio::test]
    #[ignore = "requires a host that permits Seatbelt activation"]
    async fn a_configured_write_path_does_not_grant_its_parent() {
        let root = fixture();
        let project = root.path().join("project");
        let cache = root.path().join("cache");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(&cache).unwrap();
        let output = run(
            &project,
            format!("printf cache > {}/entry", quote(&cache)),
            vec![cache.clone()],
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(cache.join("entry").exists());
        let read = run(
            &project,
            format!("cat {}/entry", quote(&cache)),
            vec![cache.clone()],
        )
        .await;
        assert!(
            read.is_error,
            "a write grant must not grant reads: {}",
            read.content
        );
        let protected = root.path().join("other");
        let output = run(
            &project,
            format!("touch {}", quote(&protected)),
            vec![cache],
        )
        .await;
        assert!(output.is_error, "{}", output.content);
        assert!(!protected.exists());
    }

    #[tokio::test]
    #[ignore = "runs installed Cargo, Python, npm and Git against temporary example projects"]
    async fn example_project_workloads() {
        workloads(true).await;
    }

    #[tokio::test]
    #[ignore = "runs installed toolchains without ilar's sandbox as an experiment baseline"]
    async fn example_project_baseline() {
        workloads(false).await;
    }

    async fn workloads(enabled: bool) {
        let root = fixture();
        let rust = root.path().join("rust");
        std::fs::create_dir_all(rust.join("src")).unwrap();
        std::fs::write(rust.join("Cargo.toml"), "[package]\nname = \"sandbox-probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[workspace]\n").unwrap();
        std::fs::write(
            rust.join("src/main.rs"),
            "fn main() { println!(\"sandbox build works\"); }\n",
        )
        .unwrap();
        let python = root.path().join("python");
        std::fs::create_dir(&python).unwrap();
        std::fs::write(python.join("main.py"), "print('sandbox build works')\n").unwrap();
        let node = root.path().join("node");
        std::fs::create_dir_all(node.join("dep")).unwrap();
        std::fs::write(node.join("package.json"), r#"{"name":"sandbox-probe","version":"1.0.0","private":true,"dependencies":{"local-dep":"file:./dep"}}"#).unwrap();
        std::fs::write(
            node.join("dep/package.json"),
            r#"{"name":"local-dep","version":"1.0.0"}"#,
        )
        .unwrap();
        let cache = root.path().join("npm-cache");
        std::fs::create_dir(&cache).unwrap();
        let mut failures = Vec::new();
        for (name, project, command, grants) in [
            (
                "cargo-offline",
                &rust,
                "env -u CARGO_TARGET_DIR RUSTFLAGS='-C linker=/usr/bin/cc' cargo build --offline && ./target/debug/sandbox-probe"
                    .to_string(),
                vec![],
            ),
            (
                "python",
                &python,
                "python3 -m py_compile main.py && python3 main.py".to_string(),
                vec![],
            ),
            (
                "npm-local",
                &node,
                format!(
                    "npm --cache {} install --offline --ignore-scripts --no-audit --no-fund",
                    quote(&cache)
                ),
                vec![cache.clone()],
            ),
            (
                "git-local",
                &node,
                "git init --quiet && git add package.json && git diff --cached --stat".to_string(),
                vec![],
            ),
        ] {
            let mut ctx = ToolContext::root(project.to_path_buf());
            ctx.sandbox.enabled = enabled;
            ctx.sandbox.read_paths = grants.clone();
            if name == "cargo-offline" {
                // The host has Cargo configuration; it is not a safe default
                // grant because such files can contain registry credentials.
                let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
                ctx.sandbox.read_paths.extend([home.join(".cargo/config.toml"), home.join(".cargo/config")]);
            }
            if name == "git-local" {
                let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
                ctx.sandbox.read_paths.extend([home.join(".gitconfig"), home.join(".config/git/config")]);
            }
            ctx.sandbox.write_paths = grants;
            let output = BashTool
                .run(json!({"command": command, "timeout_ms": 120000}), ctx)
                .await;
            println!(
                "EXPERIMENT {name} sandbox={enabled}: {}\n{}",
                if output.is_error { "FAIL" } else { "PASS" },
                output.content
            );
            if output.is_error {
                failures.push(name);
            }
        }
        assert!(
            failures.is_empty(),
            "failed workloads: {}",
            failures.join(", ")
        );
    }
}
