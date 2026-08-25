use std::sync::Arc;

use ilar::tools::web::WebFetchTool;
use ilar::tools::{ToolConcurrency, ToolContext, ToolRegistry, WorkspaceAccess};

fn ctx(dir: &std::path::Path) -> ToolContext {
    ToolContext::root(dir.to_path_buf())
}

fn registry() -> ToolRegistry {
    ToolRegistry::builtin()
}

async fn run(
    reg: &ToolRegistry,
    name: &str,
    input: serde_json::Value,
    ctx: &ToolContext,
) -> ilar::tools::ToolOutput {
    let tool = reg.get(name).unwrap_or_else(|| panic!("no tool {name}"));
    tool.run(input, ctx.clone()).await
}

// ---- kinds ----

#[test]
fn tool_capabilities_separate_concurrency_from_workspace_access() {
    let reg = registry();
    for (name, concurrency, access) in [
        (
            "read",
            ToolConcurrency::Concurrent,
            WorkspaceAccess::ReadOnly,
        ),
        (
            "glob",
            ToolConcurrency::Concurrent,
            WorkspaceAccess::ReadOnly,
        ),
        (
            "grep",
            ToolConcurrency::Concurrent,
            WorkspaceAccess::ReadOnly,
        ),
        ("write", ToolConcurrency::Barrier, WorkspaceAccess::Mutating),
        ("edit", ToolConcurrency::Barrier, WorkspaceAccess::Mutating),
        ("bash", ToolConcurrency::Barrier, WorkspaceAccess::Mutating),
        (
            "webfetch",
            ToolConcurrency::Concurrent,
            WorkspaceAccess::None,
        ),
    ] {
        let tool = reg.get(name).unwrap();
        assert_eq!(tool.concurrency(), concurrency, "{name}");
        assert_eq!(tool.workspace_access(), access, "{name}");
    }
    assert!(reg.get("nope").is_none());
}

#[test]
fn registry_exposes_provider_definitions() {
    let defs = registry().definitions();
    assert_eq!(defs.len(), 7);
    assert!(defs.iter().all(|d| d.input_schema.get("type").is_some()));
}

#[test]
fn read_only_registry_excludes_mutating_and_delegating_tools() {
    let registry = ToolRegistry::read_only();
    for name in ["read", "glob", "grep", "webfetch"] {
        assert!(registry.get(name).is_some(), "missing {name}");
    }
    for name in ["write", "edit", "bash", "task"] {
        assert!(
            registry.get(name).is_none(),
            "read-only registry exposed {name}"
        );
    }
}

#[test]
fn registry_rejects_duplicate_tool_names() {
    let error = ToolRegistry::builtin()
        .with_tool(Arc::new(WebFetchTool::default()))
        .err()
        .expect("duplicate webfetch must fail");
    assert_eq!(error.tool_name(), "webfetch");
}

#[test]
fn web_tools_always_include_websearch() {
    let registry = ToolRegistry::builtin().with_web_tools().unwrap();
    assert!(
        registry.get("websearch").is_some(),
        "websearch must register out of the box (keyless Exa fallback)"
    );
}

#[test]
fn composed_web_registry_has_unique_definitions() {
    let registry = ToolRegistry::builtin().with_web_tools().unwrap();
    let definitions = registry.definitions();
    let names: std::collections::HashSet<_> = definitions.iter().map(|tool| &tool.name).collect();
    assert_eq!(names.len(), definitions.len());
}

// ---- read ----

#[tokio::test]
async fn read_returns_numbered_lines() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("notes.txt"), "first\nsecond\n").unwrap();
    let out = run(
        &registry(),
        "read",
        serde_json::json!({"path": "notes.txt"}),
        &ctx(dir.path()),
    )
    .await;
    assert!(!out.is_error);
    assert!(out.content.contains("1→first"), "got: {}", out.content);
    assert!(out.content.contains("2→second"), "got: {}", out.content);
}

#[tokio::test]
async fn read_missing_file_is_error() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(
        &registry(),
        "read",
        serde_json::json!({"path": "nope.txt"}),
        &ctx(dir.path()),
    )
    .await;
    assert!(out.is_error);
    assert!(out.content.contains("nope.txt"));
}

#[tokio::test]
async fn read_honors_offset_and_limit() {
    let dir = tempfile::tempdir().unwrap();
    let content: Vec<String> = (1..=50).map(|i| i.to_string()).collect();
    std::fs::write(dir.path().join("n.txt"), content.join("\n")).unwrap();
    let out = run(
        &registry(),
        "read",
        serde_json::json!({"path": "n.txt", "offset": 10, "limit": 3}),
        &ctx(dir.path()),
    )
    .await;
    assert!(out.content.contains("10→10"));
    assert!(out.content.contains("12→12"));
    assert!(!out.content.contains("13→13"));
    assert!(!out.content.contains("9→9"));
}

#[tokio::test]
async fn read_windows_files_larger_than_output_cap() {
    let dir = tempfile::tempdir().unwrap();
    let mut content = "padding\n".repeat(40_000);
    content.push_str("wanted-one\nwanted-two\n");
    std::fs::write(dir.path().join("large.txt"), content).unwrap();
    let out = run(
        &registry(),
        "read",
        serde_json::json!({"path": "large.txt", "offset": 40001, "limit": 2}),
        &ctx(dir.path()),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("40001→wanted-one"), "{}", out.content);
    assert!(out.content.contains("40002→wanted-two"), "{}", out.content);
}

#[tokio::test]
async fn read_distinguishes_empty_file_from_offset_past_end() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("empty.txt"), "").unwrap();
    std::fs::write(dir.path().join("short.txt"), "one\ntwo\n").unwrap();

    let empty = run(
        &registry(),
        "read",
        serde_json::json!({"path": "empty.txt", "offset": 10}),
        &ctx(dir.path()),
    )
    .await;
    assert!(!empty.is_error);
    assert!(empty.content.contains("empty file"), "{}", empty.content);

    let past_end = run(
        &registry(),
        "read",
        serde_json::json!({"path": "short.txt", "offset": 10}),
        &ctx(dir.path()),
    )
    .await;
    assert!(past_end.is_error);
    assert!(
        past_end.content.contains("beyond end of file (2 lines)"),
        "{}",
        past_end.content
    );
}

// ---- write ----

#[tokio::test]
async fn write_creates_file_and_parent_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(
        &registry(),
        "write",
        serde_json::json!({"path": "deep/nested/file.txt", "content": "hello"}),
        &ctx(dir.path()),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("deep/nested/file.txt")).unwrap(),
        "hello"
    );
}

#[tokio::test]
async fn cancelled_write_preserves_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("existing.txt");
    std::fs::write(&path, "original").unwrap();
    let context = ctx(dir.path());
    context.cancel.cancel();

    let out = run(
        &registry(),
        "write",
        serde_json::json!({"path": "existing.txt", "content": "replacement"}),
        &context,
    )
    .await;

    assert!(out.is_error, "{}", out.content);
    assert!(out.content.contains("cancelled"), "{}", out.content);
    assert_eq!(std::fs::read_to_string(path).unwrap(), "original");
}

// ---- edit ----

#[tokio::test]
async fn edit_replaces_unique_match() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "alpha beta gamma\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    }
    let out = run(
        &registry(),
        "edit",
        serde_json::json!({"path": "a.txt", "old_string": "beta", "new_string": "BETA"}),
        &ctx(dir.path()),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "alpha BETA gamma\n"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(std::fs::metadata(path).unwrap().mode() & 0o777, 0o640);
    }
}

#[tokio::test]
async fn edit_rejects_files_above_the_size_cap_without_loading_them() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("huge.txt");
    // One byte over the 16 MiB cap.
    let content = "x".repeat(16 * 1024 * 1024 + 1);
    std::fs::write(&path, &content).unwrap();
    let out = run(
        &registry(),
        "edit",
        serde_json::json!({"path": "huge.txt", "old_string": "x", "new_string": "y"}),
        &ctx(dir.path()),
    )
    .await;
    assert!(out.is_error, "{}", out.content);
    assert!(out.content.contains("too large"), "{}", out.content);
    assert_eq!(std::fs::read_to_string(&path).unwrap().len(), content.len());
}

#[tokio::test]
async fn cancelled_edit_preserves_the_original_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("existing.txt");
    std::fs::write(&path, "original").unwrap();
    let context = ctx(dir.path());
    context.cancel.cancel();

    let out = run(
        &registry(),
        "edit",
        serde_json::json!({
            "path": "existing.txt",
            "old_string": "original",
            "new_string": "replacement"
        }),
        &context,
    )
    .await;

    assert!(out.is_error, "{}", out.content);
    assert!(out.content.contains("cancelled"), "{}", out.content);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
}

#[tokio::test]
async fn edit_ambiguous_match_errors_without_replace_all() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "x x x\n").unwrap();
    let out = run(
        &registry(),
        "edit",
        serde_json::json!({"path": "a.txt", "old_string": "x", "new_string": "y"}),
        &ctx(dir.path()),
    )
    .await;
    assert!(out.is_error);
    assert!(
        out.content.to_lowercase().contains("3"),
        "should mention match count: {}",
        out.content
    );
    // File untouched.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "x x x\n"
    );
}

#[tokio::test]
async fn edit_no_match_errors() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "content\n").unwrap();
    let out = run(
        &registry(),
        "edit",
        serde_json::json!({"path": "a.txt", "old_string": "zzz", "new_string": "y"}),
        &ctx(dir.path()),
    )
    .await;
    assert!(out.is_error);
}

#[tokio::test]
async fn edit_replace_all() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "x x x\n").unwrap();
    let out = run(
        &registry(),
        "edit",
        serde_json::json!({"path": "a.txt", "old_string": "x", "new_string": "y", "replace_all": true}),
        &ctx(dir.path()),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "y y y\n"
    );
}

// ---- bash ----

#[tokio::test]
async fn bash_runs_in_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(
        &registry(),
        "bash",
        serde_json::json!({"command": "pwd"}),
        &ctx(dir.path()),
    )
    .await;
    assert!(
        out.content.contains(dir.path().to_str().unwrap()),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn bash_drains_output_larger_than_pipe_buffer() {
    // >64KB of output would deadlock if the pipes weren't drained
    // concurrently with wait(): the child would block on the full pipe,
    // never exit, and only the 10s timeout would break the deadlock.
    // Proof of draining: fast, successful exit (output truncated to the
    // 100KB cap by design, which is fine).
    let dir = tempfile::tempdir().unwrap();
    let start = std::time::Instant::now();
    let out = run(
        &registry(),
        "bash",
        serde_json::json!({"command": "yes 0123456789 | head -c 300000", "timeout_ms": 10000}),
        &ctx(dir.path()),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.len() >= 90_000,
        "output suspiciously small: {}",
        out.content.len()
    );
    assert!(out.content.len() < 110_000, "output was not bounded");
    assert!(
        start.elapsed() < std::time::Duration::from_secs(8),
        "looked like a pipe deadlock: {:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn bash_drains_stderr_while_stdout_remains_open() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(
        &registry(),
        "bash",
        serde_json::json!({
            "command": "yes stderr | head -c 200000 >&2; printf done",
            "timeout_ms": 5000
        }),
        &ctx(dir.path()),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("done"), "{}", out.content);
}

#[tokio::test]
async fn bash_drains_high_volume_stdout_and_stderr_together() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(
        &registry(),
        "bash",
        serde_json::json!({
            "command": "(yes out | head -c 200000) & (yes err | head -c 200000 >&2) & wait",
            "timeout_ms": 5000
        }),
        &ctx(dir.path()),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.len() < 110_000, "output was not bounded");
}

#[tokio::test]
async fn bash_keeps_stderr_and_the_stdout_tail_when_stdout_fills_the_cap() {
    // The failure of a chatty build lives in the last stdout lines and in
    // stderr; keeping the first 100KB of the concatenation loses both.
    let dir = tempfile::tempdir().unwrap();
    let out = run(
        &registry(),
        "bash",
        serde_json::json!({
            "command": "yes 0123456789 | head -c 300000; printf 'stdout-tail\\n'; \
                        printf 'fatal: the real error\\n' >&2; exit 1",
            "timeout_ms": 10000
        }),
        &ctx(dir.path()),
    )
    .await;
    assert!(out.is_error, "{}", out.content);
    assert!(
        out.content.contains("fatal: the real error"),
        "stderr was dropped: {}",
        &out.content[out.content.len().saturating_sub(400)..]
    );
    assert!(
        out.content.contains("stdout-tail"),
        "stdout tail was dropped: {}",
        &out.content[out.content.len().saturating_sub(400)..]
    );
    assert!(out.content.len() < 110_000, "output was not bounded");
    // 300000 + "stdout-tail\n" (12) + "fatal: the real error\n" (22)
    assert!(
        out.content.contains("from 300034 raw bytes"),
        "raw byte total misreported: {}",
        &out.content[out.content.len().saturating_sub(400)..]
    );
}

#[tokio::test]
async fn bash_truncates_only_at_utf8_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(
        &registry(),
        "bash",
        serde_json::json!({
            "command": "yes a | tr -d '\\n' | head -c 102399; printf 'é-tail'",
            "timeout_ms": 5000
        }),
        &ctx(dir.path()),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("output truncated"));
}

#[tokio::test]
async fn bash_preserves_invalid_utf8_lossily() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(
        &registry(),
        "bash",
        serde_json::json!({"command": "printf '\\377ok'"}),
        &ctx(dir.path()),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("ok"), "{}", out.content);
    assert!(out.content.contains('�'), "{}", out.content);
}

#[tokio::test]
async fn bash_nonzero_exit_is_error_with_output_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(
        &registry(),
        "bash",
        serde_json::json!({"command": "echo useful-output; exit 3"}),
        &ctx(dir.path()),
    )
    .await;
    assert!(out.is_error);
    assert!(out.content.contains("useful-output"));
    assert!(out.content.contains('3'));
}

#[tokio::test]
async fn bash_timeout_kills() {
    let dir = tempfile::tempdir().unwrap();
    let start = std::time::Instant::now();
    let out = run(
        &registry(),
        "bash",
        serde_json::json!({"command": "printf partial-output; sleep 30", "timeout_ms": 300}),
        &ctx(dir.path()),
    )
    .await;
    assert!(out.is_error);
    assert!(out.content.to_lowercase().contains("timed out"));
    assert!(out.content.contains("partial-output"), "{}", out.content);
    assert!(start.elapsed() < std::time::Duration::from_secs(10));
}

#[cfg(unix)]
#[tokio::test]
async fn bash_reports_signal_termination() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(
        &registry(),
        "bash",
        serde_json::json!({"command": "kill -TERM $$"}),
        &ctx(dir.path()),
    )
    .await;
    assert!(out.is_error);
    assert!(out.content.contains("signal 15"), "{}", out.content);
}

#[cfg(unix)]
#[tokio::test]
async fn bash_timeout_kills_descendants() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(
        &registry(),
        "bash",
        serde_json::json!({
            "command": "sleep 30 & echo $! > child.pid; wait",
            "timeout_ms": 300
        }),
        &ctx(dir.path()),
    )
    .await;
    assert!(out.is_error);
    let pid = std::fs::read_to_string(dir.path().join("child.pid"))
        .unwrap()
        .trim()
        .to_string();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        !std::process::Command::new("kill")
            .args(["-0", &pid])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success(),
        "descendant process {pid} survived timeout"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn dropping_bash_future_kills_descendants() {
    let dir = tempfile::tempdir().unwrap();
    let tool = registry().get("bash").unwrap();
    let cwd = dir.path().to_path_buf();
    let handle = tokio::spawn(tool.run(
        serde_json::json!({"command": "sleep 30 & echo $! > child.pid; wait"}),
        ToolContext::root(cwd.clone()),
    ));
    let pid_path = cwd.join("child.pid");
    for _ in 0..50 {
        if pid_path.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let pid = std::fs::read_to_string(pid_path)
        .unwrap()
        .trim()
        .to_string();
    handle.abort();
    let _ = handle.await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        !std::process::Command::new("kill")
            .args(["-0", &pid])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success(),
        "descendant process {pid} survived future drop"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn successful_bash_kills_daemonized_process_group_children() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("daemon-marker");
    let command = format!("(sleep 1; touch {}) >/dev/null 2>&1 &", marker.display());
    let out = run(
        &registry(),
        "bash",
        serde_json::json!({"command": command}),
        &ctx(dir.path()),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    assert!(
        !marker.exists(),
        "background descendant survived Bash completion"
    );
}

// ---- glob ----

#[tokio::test]
async fn glob_matches_nested_patterns() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src/deep")).unwrap();
    std::fs::write(dir.path().join("src/main.rs"), "").unwrap();
    std::fs::write(dir.path().join("src/deep/mod.rs"), "").unwrap();
    std::fs::write(dir.path().join("README.md"), "").unwrap();
    let out = run(
        &registry(),
        "glob",
        serde_json::json!({"pattern": "src/**/*.rs"}),
        &ctx(dir.path()),
    )
    .await;
    assert!(!out.is_error);
    assert!(out.content.contains("src/main.rs"), "{}", out.content);
    assert!(out.content.contains("src/deep/mod.rs"), "{}", out.content);
    assert!(!out.content.contains("README.md"));
}

#[tokio::test]
async fn glob_skips_ignored_paths_by_default_and_can_include_them() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::create_dir_all(dir.path().join("build")).unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    std::fs::write(dir.path().join(".gitignore"), "build/\n").unwrap();
    std::fs::write(dir.path().join("src/a.js"), "").unwrap();
    std::fs::write(dir.path().join("build/out.js"), "").unwrap();
    std::fs::write(dir.path().join(".git/hook.js"), "").unwrap();

    let out = run(
        &registry(),
        "glob",
        serde_json::json!({"pattern": "**/*.js"}),
        &ctx(dir.path()),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("src/a.js"), "{}", out.content);
    assert!(!out.content.contains("build/out.js"), "{}", out.content);
    assert!(!out.content.contains("hook.js"), "{}", out.content);

    let all = run(
        &registry(),
        "glob",
        serde_json::json!({"pattern": "**/*.js", "include_ignored": true}),
        &ctx(dir.path()),
    )
    .await;
    assert!(!all.is_error, "{}", all.content);
    assert!(all.content.contains("src/a.js"), "{}", all.content);
    assert!(all.content.contains("build/out.js"), "{}", all.content);
}

#[tokio::test]
async fn glob_still_matches_dotted_paths_the_pattern_asks_for() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".github/workflows")).unwrap();
    std::fs::write(dir.path().join(".github/workflows/ci.yml"), "").unwrap();
    let out = run(
        &registry(),
        "glob",
        serde_json::json!({"pattern": ".github/workflows/*.yml"}),
        &ctx(dir.path()),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.contains(".github/workflows/ci.yml"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn glob_rejects_patterns_that_escape_the_workspace() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    for pattern in ["../*.rs", "/etc/*", "src/../../*.rs"] {
        let out = run(
            &registry(),
            "glob",
            serde_json::json!({ "pattern": pattern }),
            &ctx(dir.path()),
        )
        .await;
        assert!(
            out.is_error,
            "{pattern} should be rejected: {}",
            out.content
        );
        assert!(out.content.contains("workspace"), "{}", out.content);
    }
}

// ---- grep ----

#[tokio::test]
async fn grep_finds_matches_with_file_and_line() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/a.rs"), "fn main() {}\n// TODO fix\n").unwrap();
    std::fs::write(dir.path().join("src/b.rs"), "let x = 1;\n").unwrap();
    let out = run(
        &registry(),
        "grep",
        serde_json::json!({"pattern": "TODO"}),
        &ctx(dir.path()),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("src/a.rs:2"), "{}", out.content);
    assert!(!out.content.contains("b.rs"));
}

#[tokio::test]
async fn grep_and_glob_agree_on_which_files_exist() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".github/workflows")).unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::create_dir_all(dir.path().join("ignored")).unwrap();
    std::fs::write(dir.path().join(".gitignore"), "ignored/\n").unwrap();
    std::fs::write(dir.path().join(".github/workflows/ci.yml"), "on: NEEDLE\n").unwrap();
    std::fs::write(dir.path().join(".env"), "SECRET=NEEDLE\n").unwrap();
    std::fs::write(dir.path().join("src/a.rs"), "// NEEDLE\n").unwrap();
    std::fs::write(dir.path().join("ignored/x.rs"), "// NEEDLE\n").unwrap();

    let out = run(
        &registry(),
        "grep",
        serde_json::json!({"pattern": "NEEDLE"}),
        &ctx(dir.path()),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    // Dotted paths are where CI config and env files live.
    assert!(
        out.content.contains(".github/workflows/ci.yml:1"),
        "{}",
        out.content
    );
    assert!(out.content.contains(".env:1"), "{}", out.content);
    assert!(out.content.contains("src/a.rs:1"), "{}", out.content);
    // The docstring promises gitignored files are skipped.
    assert!(!out.content.contains("ignored/x.rs"), "{}", out.content);

    let all = run(
        &registry(),
        "grep",
        serde_json::json!({"pattern": "NEEDLE", "include_ignored": true}),
        &ctx(dir.path()),
    )
    .await;
    assert!(all.content.contains("ignored/x.rs:1"), "{}", all.content);
}

#[tokio::test]
async fn grep_orders_results_by_path_then_line() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/b.rs"), "NEEDLE\n").unwrap();
    std::fs::write(dir.path().join("src/a.rs"), "x\nNEEDLE\nNEEDLE\n").unwrap();
    let out = run(
        &registry(),
        "grep",
        serde_json::json!({"pattern": "NEEDLE"}),
        &ctx(dir.path()),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    let lines: Vec<&str> = out.content.lines().collect();
    assert_eq!(
        lines,
        vec![
            "src/a.rs:2:NEEDLE",
            "src/a.rs:3:NEEDLE",
            "src/b.rs:1:NEEDLE"
        ],
        "{}",
        out.content
    );
}

#[tokio::test]
async fn grep_rejects_paths_that_escape_the_workspace() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
    for path in ["..", "/etc", "a/../.."] {
        let out = run(
            &registry(),
            "grep",
            serde_json::json!({"pattern": "hello", "path": path}),
            &ctx(dir.path()),
        )
        .await;
        assert!(out.is_error, "{path} should be rejected: {}", out.content);
        assert!(out.content.contains("workspace"), "{}", out.content);
    }
}

#[tokio::test]
async fn grep_no_matches_is_not_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
    let out = run(
        &registry(),
        "grep",
        serde_json::json!({"pattern": "zzzzz"}),
        &ctx(dir.path()),
    )
    .await;
    assert!(!out.is_error);
    assert!(out.content.trim().is_empty());
}

#[tokio::test]
async fn grep_respects_path_filter() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("include")).unwrap();
    std::fs::create_dir_all(dir.path().join("exclude")).unwrap();
    std::fs::write(dir.path().join("include/x.txt"), "needle\n").unwrap();
    std::fs::write(dir.path().join("exclude/y.txt"), "needle\n").unwrap();
    let out = run(
        &registry(),
        "grep",
        serde_json::json!({"pattern": "needle", "path": "include"}),
        &ctx(dir.path()),
    )
    .await;
    assert!(out.content.contains("include/x.txt"), "{}", out.content);
    assert!(!out.content.contains("exclude"));
}

#[tokio::test]
async fn grep_bounds_long_lines_and_total_output() {
    let dir = tempfile::tempdir().unwrap();
    for index in 0..80 {
        std::fs::write(
            dir.path().join(format!("match-{index}.txt")),
            format!("{} needle\n", "x".repeat(20_000)),
        )
        .unwrap();
    }
    let out = run(
        &registry(),
        "grep",
        serde_json::json!({"pattern": "needle"}),
        &ctx(dir.path()),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.len() <= 256 * 1024, "{}", out.content.len());
    assert!(
        out.content.lines().all(|line| line.len() <= 8 * 1024 + 3),
        "grep retained an unbounded line"
    );

    std::fs::write(
        dir.path().join("over-file-cap.txt"),
        format!("needle {}", "x".repeat(2 * 1024 * 1024)),
    )
    .unwrap();
    let out = run(
        &registry(),
        "grep",
        serde_json::json!({"pattern": "needle", "path": "over-file-cap.txt"}),
        &ctx(dir.path()),
    )
    .await;
    assert!(
        out.content.contains("over-file-cap.txt:1:needle"),
        "{}",
        out.content
    );
    assert!(out.content.contains("truncated"), "{}", out.content);
}

#[tokio::test]
async fn grep_matches_anchors_without_line_terminators_or_cap_sentinels() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("endings.txt"), "foo\nfoo\r\nbar\n").unwrap();
    let anchored = run(
        &registry(),
        "grep",
        serde_json::json!({"pattern": "foo$", "path": "endings.txt"}),
        &ctx(dir.path()),
    )
    .await;
    assert_eq!(anchored.content.lines().count(), 2, "{}", anchored.content);

    let mut capped = vec![b'x'; 2 * 1024 * 1024 - 1];
    capped.extend_from_slice(b"\nbeyond\n");
    std::fs::write(dir.path().join("capped.txt"), capped).unwrap();
    let empty = run(
        &registry(),
        "grep",
        serde_json::json!({"pattern": "^$", "path": "capped.txt"}),
        &ctx(dir.path()),
    )
    .await;
    assert!(!empty.content.contains("capped.txt:"), "{}", empty.content);
    assert!(empty.content.contains("truncated"), "{}", empty.content);
}

#[tokio::test]
async fn glob_stops_at_its_match_cap() {
    let dir = tempfile::tempdir().unwrap();
    for index in 0..1005 {
        std::fs::write(dir.path().join(format!("file-{index:04}.txt")), "").unwrap();
    }
    let out = run(
        &registry(),
        "glob",
        serde_json::json!({"pattern": "*.txt"}),
        &ctx(dir.path()),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(out.content.lines().count(), 1001);
    assert!(out.content.contains("truncated at 1000 matches"));
}

#[test]
fn restricted_registry_intersects_with_the_base_set() {
    let restricted =
        ToolRegistry::builtin().restricted_to(&["grep".to_string(), "read".into(), "edit".into()]);
    let mut names = restricted.tool_names();
    names.sort_unstable();
    assert_eq!(names, ["edit", "grep", "read"]);

    // Intersection: allowlisted tools missing from a read-only base are
    // simply not granted, and excluded tools are gone.
    let read_only = ToolRegistry::read_only().restricted_to(&[
        "grep".to_string(),
        "edit".into(),
        "bash".into(),
    ]);
    assert_eq!(read_only.tool_names(), ["grep"]);
    assert!(read_only.get("edit").is_none());
    assert!(read_only.get("bash").is_none());

    let known = ilar::tools::child_tool_names();
    for name in [
        "read", "write", "edit", "bash", "glob", "grep", "webfetch", "task",
    ] {
        assert!(
            known.contains(&name),
            "{name} missing from child_tool_names"
        );
    }
}

#[tokio::test]
async fn running_bash_reports_a_live_output_tail() {
    let dir = tempfile::tempdir().unwrap();
    let (sender, mut receiver) = ilar::agent::loop_event_channel(16);
    let mut ctx = ctx(dir.path());
    ctx.call_id = Some("bash-tail-1".into());
    ctx.output_tail = Some(sender.output_tail_sink());

    let reg = registry();
    let output = run(
        &reg,
        "bash",
        serde_json::json!({"command": "echo progress-marker; sleep 1.4; echo done"}),
        &ctx,
    )
    .await;
    assert!(!output.is_error, "{}", output.content);

    let mut tails = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        if let ilar::agent::LoopEvent::ToolOutputTail { id, tail } = event {
            assert_eq!(id, "bash-tail-1");
            tails.push(tail);
        }
    }
    assert!(
        tails.iter().any(|tail| tail.contains("progress-marker")),
        "expected a live tail before completion: {tails:?}"
    );
    assert!(
        !tails.iter().any(|tail| tail.contains("done")) || tails.len() > 1,
        "tail should have been reported while the command was still running"
    );
}

#[tokio::test]
async fn history_searches_this_session_and_no_other() {
    let dir = tempfile::tempdir().unwrap();
    let store = ilar::session::SessionStore::new(dir.path().to_path_buf());
    let mut ids = Vec::new();
    for (marker, text) in [
        ("mine", "the AES table lives at offset 0x4f11b4"),
        ("theirs", "a secret belonging to another session"),
    ] {
        let id = ilar::session::new_id();
        let mut session = store
            .create(ilar::session::SessionMeta {
                session_id: id.clone(),
                parent_id: None,
                agent: "build".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::UserMessage {
                id: ilar::session::new_id(),
                text: format!("{marker}: {text}"),
                images: Vec::new(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(session);
        ids.push(id);
    }

    let registry = ToolRegistry::builtin().with_history(store).unwrap();
    let history = registry.get("history").unwrap();
    let context = |session: &str| {
        let mut ctx = ToolContext::root(std::env::temp_dir());
        ctx.session_id = session.to_string();
        ctx
    };

    let found = history
        .run(serde_json::json!({"query": "0x4f11b4"}), context(&ids[0]))
        .await;
    assert!(!found.is_error, "{}", found.content);
    assert!(found.content.contains("0x4f11b4"), "{}", found.content);
    assert!(found.content.contains("event 1"), "{}", found.content);

    // Another session's log is not reachable.
    let leak = history
        .run(serde_json::json!({"query": "secret"}), context(&ids[0]))
        .await;
    assert!(
        leak.content.contains("no earlier mention"),
        "{}",
        leak.content
    );

    // A speaker lists what they said, with no query at all: "what was
    // I actually asked?" in one call, which is why the handover does
    // not carry the request verbatim.
    let asked = history
        .run(serde_json::json!({"speaker": "user"}), context(&ids[0]))
        .await;
    assert!(!asked.is_error, "{}", asked.content);
    assert!(asked.content.contains("mine:"), "{}", asked.content);
    assert!(!asked.content.contains("theirs:"), "{}", asked.content);

    // A speaker narrows a search.
    let narrowed = history
        .run(
            serde_json::json!({"query": "0x4f11b4", "speaker": "tool_result"}),
            context(&ids[0]),
        )
        .await;
    assert!(
        narrowed.content.contains("no earlier mention"),
        "the user's line matched a tool-result search: {}",
        narrowed.content
    );

    let unknown = history
        .run(serde_json::json!({"speaker": "nobody"}), context(&ids[0]))
        .await;
    assert!(unknown.is_error, "{}", unknown.content);

    // An event index reads the conversation around it.
    let around = history
        .run(serde_json::json!({"event": 1}), context(&ids[0]))
        .await;
    assert!(around.content.contains("AES table"), "{}", around.content);

    // Outside a session, and with neither argument, it says so.
    let homeless = history
        .run(
            serde_json::json!({"query": "anything"}),
            ToolContext::root(std::env::temp_dir()),
        )
        .await;
    assert!(homeless.is_error, "{}", homeless.content);
    let empty = history.run(serde_json::json!({}), context(&ids[0])).await;
    assert!(empty.is_error, "{}", empty.content);
}
