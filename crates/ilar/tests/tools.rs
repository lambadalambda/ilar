use ilar::tools::{ToolContext, ToolKind, ToolRegistry};

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
fn tool_kinds_declared() {
    let reg = registry();
    assert_eq!(reg.get("read").unwrap().kind(), ToolKind::ReadOnly);
    assert_eq!(reg.get("glob").unwrap().kind(), ToolKind::ReadOnly);
    assert_eq!(reg.get("grep").unwrap().kind(), ToolKind::ReadOnly);
    assert_eq!(reg.get("write").unwrap().kind(), ToolKind::Mutating);
    assert_eq!(reg.get("edit").unwrap().kind(), ToolKind::Mutating);
    assert_eq!(reg.get("bash").unwrap().kind(), ToolKind::Mutating);
    assert!(reg.get("nope").is_none());
}

#[test]
fn registry_exposes_provider_definitions() {
    let defs = registry().definitions();
    assert_eq!(defs.len(), 6);
    assert!(defs.iter().all(|d| d.input_schema.get("type").is_some()));
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

// ---- edit ----

#[tokio::test]
async fn edit_replaces_unique_match() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha beta gamma\n").unwrap();
    let out = run(
        &registry(),
        "edit",
        serde_json::json!({"path": "a.txt", "old_string": "beta", "new_string": "BETA"}),
        &ctx(dir.path()),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "alpha BETA gamma\n"
    );
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
    assert!(
        start.elapsed() < std::time::Duration::from_secs(8),
        "looked like a pipe deadlock: {:?}",
        start.elapsed()
    );
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
        serde_json::json!({"command": "sleep 30", "timeout_ms": 300}),
        &ctx(dir.path()),
    )
    .await;
    assert!(out.is_error);
    assert!(out.content.to_lowercase().contains("timed out"));
    assert!(start.elapsed() < std::time::Duration::from_secs(10));
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
