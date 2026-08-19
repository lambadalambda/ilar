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
