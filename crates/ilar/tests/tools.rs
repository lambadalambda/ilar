use std::sync::Arc;

use ilar::tools::web::WebFetchTool;
use ilar::tools::{ToolConcurrency, ToolContext, ToolRegistry, WorkspaceAccess};

fn ctx(dir: &std::path::Path) -> ToolContext {
    ToolContext::root(dir.to_path_buf())
}

fn registry() -> ToolRegistry {
    ToolRegistry::builtin()
}

/// A context that spills oversized tool output, under a known session
/// and call id — what the executor hands a tool during a real turn.
fn spilling_ctx(cwd: &std::path::Path, spill_dir: &std::path::Path, call_id: &str) -> ToolContext {
    let mut ctx = ToolContext::root(cwd.to_path_buf()).with_spill_dir(spill_dir.to_path_buf());
    ctx.session_id = "session-1".into();
    ctx.call_id = Some(call_id.to_string());
    ctx
}

/// The path out of the `full output: <path> (…)` hint, which leads the
/// result: the TUI cuts a tool result head-first, so a hint anywhere
/// else is one only the model would see.
fn hinted_spill_path(content: &str) -> std::path::PathBuf {
    let hint = content
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("full output: "))
        .unwrap_or_else(|| panic!("no leading spill hint in: {content}"));
    let (path, rest) = hint
        .rsplit_once(" (")
        .unwrap_or_else(|| panic!("malformed spill hint: {hint}"));
    assert!(
        rest.contains("lines) — grep or read it for what you need"),
        "malformed spill hint: {hint}"
    );
    std::path::PathBuf::from(path)
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

/// What a model does before editing, and what edit now requires: read the
/// file through this very context.
async fn read_first(reg: &ToolRegistry, path: &str, ctx: &ToolContext) {
    let out = run(reg, "read", serde_json::json!({"path": path}), ctx).await;
    assert!(!out.is_error, "priming read failed: {}", out.content);
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

/// Steering, not machinery: the two descriptions are the only thing
/// standing between a model and a fetched guess.
#[test]
fn web_tool_descriptions_route_unknown_urls_through_websearch() {
    let registry = ToolRegistry::builtin().with_web_tools().unwrap();

    let webfetch = registry.get("webfetch").unwrap().description();
    assert!(webfetch.contains("guessed URLs mostly 404"), "{webfetch}");
    assert!(
        webfetch.contains("websearch for the page first"),
        "{webfetch}"
    );

    let websearch = registry.get("websearch").unwrap().description();
    assert!(
        websearch.contains("the real pages webfetch should fetch"),
        "{websearch}"
    );
    assert!(
        websearch.contains("instead of guessing a URL"),
        "{websearch}"
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

fn png_bytes(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend_from_slice(&13_u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
    bytes.extend_from_slice(&[0x1f, 0x15, 0xc4, 0x89]);
    bytes.extend_from_slice(&(0..=255_u32).map(|b| b as u8).collect::<Vec<u8>>());
    bytes
}

#[tokio::test]
async fn read_describes_png_instead_of_returning_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = png_bytes(48, 32);
    std::fs::write(dir.path().join("shot.png"), &bytes).unwrap();
    let out = run(
        &registry(),
        "read",
        serde_json::json!({"path": "shot.png"}),
        &ctx(dir.path()),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("shot.png"), "{}", out.content);
    assert!(out.content.contains("PNG image"), "{}", out.content);
    assert!(out.content.contains("48x32"), "{}", out.content);
    assert!(
        out.content.contains(&format!("{} bytes", bytes.len())),
        "{}",
        out.content
    );
    assert!(
        out.content.contains("cannot be read as text"),
        "{}",
        out.content
    );
    assert!(out.content.lines().count() == 1, "{}", out.content);
    assert!(!out.content.contains('\u{fffd}'), "{}", out.content);
    assert!(!out.content.contains("1→"), "{}", out.content);
}

#[tokio::test]
async fn read_describes_png_even_with_offset_and_limit() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("shot.png"), png_bytes(4, 4)).unwrap();
    let out = run(
        &registry(),
        "read",
        serde_json::json!({"path": "shot.png", "offset": 2, "limit": 5}),
        &ctx(dir.path()),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("PNG image"), "{}", out.content);
}

#[tokio::test]
async fn read_describes_generic_binary_file() {
    let dir = tempfile::tempdir().unwrap();
    let bytes: Vec<u8> = (0..4096).map(|i| (i % 256) as u8).collect();
    std::fs::write(dir.path().join("blob.bin"), &bytes).unwrap();
    let out = run(
        &registry(),
        "read",
        serde_json::json!({"path": "blob.bin"}),
        &ctx(dir.path()),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("blob.bin"), "{}", out.content);
    assert!(out.content.contains("binary data"), "{}", out.content);
    assert!(out.content.contains("4096 bytes"), "{}", out.content);
    assert!(
        out.content.contains("cannot be read as text"),
        "{}",
        out.content
    );
    assert!(out.content.lines().count() == 1, "{}", out.content);
    assert!(!out.content.contains('\u{fffd}'), "{}", out.content);
}

#[tokio::test]
async fn read_keeps_utf8_source_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let source = "fn main() {\n    println!(\"héllo — 世界 🌍\");\n}\n";
    std::fs::write(dir.path().join("main.rs"), source).unwrap();
    let out = run(
        &registry(),
        "read",
        serde_json::json!({"path": "main.rs"}),
        &ctx(dir.path()),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("1→fn main() {"), "{}", out.content);
    assert!(out.content.contains("héllo — 世界 🌍"), "{}", out.content);
    assert!(!out.content.contains("binary"), "{}", out.content);
}

#[tokio::test]
async fn read_keeps_text_with_ansi_escapes_as_text() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = String::new();
    for i in 1..=20 {
        log.push_str(&format!(
            "\u{1b}[32mINFO\u{1b}[0m step {i}: \u{1b}[1;31mfailed\u{1b}[0m retrying\n"
        ));
    }
    std::fs::write(dir.path().join("run.log"), &log).unwrap();
    let out = run(
        &registry(),
        "read",
        serde_json::json!({"path": "run.log"}),
        &ctx(dir.path()),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("1→"), "{}", out.content);
    assert!(out.content.contains("step 1:"), "{}", out.content);
    assert!(!out.content.contains("binary"), "{}", out.content);
}

fn vision_ctx(dir: &std::path::Path) -> ToolContext {
    let mut ctx = ctx(dir);
    ctx.vision = true;
    ctx
}

#[tokio::test]
async fn read_returns_the_image_itself_in_a_vision_session() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = png_bytes(48, 32);
    std::fs::write(dir.path().join("shot.png"), &bytes).unwrap();
    let out = run(
        &registry(),
        "read",
        serde_json::json!({"path": "shot.png"}),
        &vision_ctx(dir.path()),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(out.images().len(), 1, "{:?}", out);
    assert_eq!(out.images()[0].media_type, "image/png");
    assert!(out.content.contains("PNG image, 48x32"), "{}", out.content);
    assert!(
        out.content.contains("the image itself follows"),
        "{}",
        out.content
    );
    assert!(
        !out.content.contains("cannot be read as text"),
        "{}",
        out.content
    );
    assert!(!out.content.contains("do not retry"), "{}", out.content);
    assert_eq!(out.content.lines().count(), 1, "{}", out.content);
}

#[tokio::test]
async fn read_returns_the_image_regardless_of_offset_and_limit() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("shot.png"), png_bytes(4, 4)).unwrap();
    let out = run(
        &registry(),
        "read",
        serde_json::json!({"path": "shot.png", "offset": 2, "limit": 5}),
        &vision_ctx(dir.path()),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(out.images().len(), 1, "{:?}", out);
    assert!(
        out.content.contains("the image itself follows"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn read_without_vision_describes_the_image_and_attaches_nothing() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("shot.png"), png_bytes(48, 32)).unwrap();
    let out = run(
        &registry(),
        "read",
        serde_json::json!({"path": "shot.png"}),
        &ctx(dir.path()),
    )
    .await;
    assert!(out.images().is_empty(), "{:?}", out);
    assert!(out.content.contains("PNG image, 48x32"), "{}", out.content);
    assert!(
        out.content
            .contains("cannot be read as text, do not retry with offset/limit"),
        "{}",
        out.content
    );
    assert!(
        !out.content.contains("the image itself follows"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn read_describes_an_oversized_image_without_decoding_it() {
    let dir = tempfile::tempdir().unwrap();
    // Size guard fires before the decode: zeros after the magic bytes are
    // not a decodable PNG, and nothing tries.
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.resize(10 * 1024 * 1024 + 1, 0);
    std::fs::write(dir.path().join("huge.png"), &bytes).unwrap();
    let out = run(
        &registry(),
        "read",
        serde_json::json!({"path": "huge.png"}),
        &vision_ctx(dir.path()),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.images().is_empty(), "{:?}", out);
    assert!(out.content.contains("PNG image"), "{}", out.content);
    assert!(
        out.content
            .contains(&format!("{} bytes", 10 * 1024 * 1024 + 1)),
        "{}",
        out.content
    );
    assert!(
        out.content.contains("cannot be read as text"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn read_attaches_a_jpeg_without_re_encoding_it() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = b"\xFF\xD8\xFF\xE0\x00\x10JFIF\x00 not really a jpeg body".to_vec();
    std::fs::write(dir.path().join("photo.jpg"), &bytes).unwrap();
    let out = run(
        &registry(),
        "read",
        serde_json::json!({"path": "photo.jpg"}),
        &vision_ctx(dir.path()),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(out.images().len(), 1, "{:?}", out);
    assert_eq!(
        out.images()[0],
        ilar::session::ImageContent::new("image/jpeg", &bytes),
        "the file's own bytes must reach the model untouched"
    );
    assert!(out.content.contains("JPEG image"), "{}", out.content);
    assert!(
        out.content.contains("the image itself follows"),
        "{}",
        out.content
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
    let context = ctx(dir.path());
    read_first(&registry(), "a.txt", &context).await;
    let out = run(
        &registry(),
        "edit",
        serde_json::json!({"path": "a.txt", "old_string": "beta", "new_string": "BETA"}),
        &context,
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
    let context = ctx(dir.path());
    read_first(&registry(), "a.txt", &context).await;
    let out = run(
        &registry(),
        "edit",
        serde_json::json!({"path": "a.txt", "old_string": "x", "new_string": "y"}),
        &context,
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
    let context = ctx(dir.path());
    read_first(&registry(), "a.txt", &context).await;
    let out = run(
        &registry(),
        "edit",
        serde_json::json!({"path": "a.txt", "old_string": "zzz", "new_string": "y"}),
        &context,
    )
    .await;
    assert!(out.is_error);
}

#[tokio::test]
async fn edit_replace_all() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "x x x\n").unwrap();
    let context = ctx(dir.path());
    read_first(&registry(), "a.txt", &context).await;
    let out = run(
        &registry(),
        "edit",
        serde_json::json!({"path": "a.txt", "old_string": "x", "new_string": "y", "replace_all": true}),
        &context,
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "y y y\n"
    );
}

// ---- edit: what the model has seen ----

#[tokio::test]
async fn edit_refuses_a_file_this_session_never_read() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();

    let out = run(
        &registry(),
        "edit",
        serde_json::json!({"path": "a.txt", "old_string": "alpha", "new_string": "beta"}),
        &ctx(dir.path()),
    )
    .await;

    assert!(out.is_error, "{}", out.content);
    assert!(
        out.content
            .contains("you have not read this file in this session; read it first"),
        "{}",
        out.content
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "alpha\n"
    );
}

#[tokio::test]
async fn edit_refuses_a_file_that_changed_since_the_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "alpha\n").unwrap();
    let context = ctx(dir.path());
    read_first(&registry(), "a.txt", &context).await;
    // Out of band: a bash command (or another process) rewrote it.
    std::fs::write(&path, "alpha\nbeta\n").unwrap();

    let out = run(
        &registry(),
        "edit",
        serde_json::json!({"path": "a.txt", "old_string": "alpha", "new_string": "gamma"}),
        &context,
    )
    .await;

    assert!(out.is_error, "{}", out.content);
    assert!(
        out.content.contains(
            "the file changed since you last read it (a command or another process wrote it); \
             re-read before editing"
        ),
        "{}",
        out.content
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "alpha\nbeta\n");
}

#[tokio::test]
async fn edit_succeeds_after_re_reading_the_changed_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "alpha\n").unwrap();
    let context = ctx(dir.path());
    read_first(&registry(), "a.txt", &context).await;
    std::fs::write(&path, "alpha\nbeta\n").unwrap();
    read_first(&registry(), "a.txt", &context).await;

    let out = run(
        &registry(),
        "edit",
        serde_json::json!({"path": "a.txt", "old_string": "alpha", "new_string": "gamma"}),
        &context,
    )
    .await;

    assert!(!out.is_error, "{}", out.content);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "gamma\nbeta\n");
}

/// The window came from this version of the file, so the model has seen
/// this version — editing outside the window is its own business.
#[tokio::test]
async fn a_windowed_read_unlocks_the_whole_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "one\ntwo\nthree\nfour\n").unwrap();
    let context = ctx(dir.path());
    let read = run(
        &registry(),
        "read",
        serde_json::json!({"path": "a.txt", "offset": 1, "limit": 2}),
        &context,
    )
    .await;
    assert!(!read.is_error, "{}", read.content);

    let out = run(
        &registry(),
        "edit",
        serde_json::json!({"path": "a.txt", "old_string": "four", "new_string": "FOUR"}),
        &context,
    )
    .await;

    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "one\ntwo\nthree\nFOUR\n"
    );
}

/// A read that returned nothing showed the model nothing.
#[tokio::test]
async fn a_failed_read_does_not_unlock_edits() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
    let context = ctx(dir.path());
    let read = run(
        &registry(),
        "read",
        serde_json::json!({"path": "a.txt", "offset": 10}),
        &context,
    )
    .await;
    assert!(read.is_error, "{}", read.content);

    let out = run(
        &registry(),
        "edit",
        serde_json::json!({"path": "a.txt", "old_string": "alpha", "new_string": "beta"}),
        &context,
    )
    .await;

    assert!(out.is_error, "{}", out.content);
    assert!(
        out.content.contains("you have not read this file"),
        "{}",
        out.content
    );
}

/// A binary file's read returns a description, not contents — the model
/// has seen nothing it could match on, so edit stays shut.
#[tokio::test]
async fn reading_a_binary_file_does_not_unlock_edits() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.bin");
    // Valid UTF-8 (so edit's own read succeeds) but binary to the sniffer.
    std::fs::write(&path, "alpha\0beta\n").unwrap();
    let context = ctx(dir.path());
    let read = run(
        &registry(),
        "read",
        serde_json::json!({"path": "a.bin"}),
        &context,
    )
    .await;
    assert!(read.content.contains("binary file"), "{}", read.content);

    let out = run(
        &registry(),
        "edit",
        serde_json::json!({"path": "a.bin", "old_string": "alpha", "new_string": "ALPHA"}),
        &context,
    )
    .await;

    assert!(out.is_error, "{}", out.content);
    assert!(
        out.content.contains("you have not read this file"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn a_written_file_can_be_edited_without_reading_it_back() {
    let dir = tempfile::tempdir().unwrap();
    let context = ctx(dir.path());
    let written = run(
        &registry(),
        "write",
        serde_json::json!({"path": "a.txt", "content": "alpha\n"}),
        &context,
    )
    .await;
    assert!(!written.is_error, "{}", written.content);

    let out = run(
        &registry(),
        "edit",
        serde_json::json!({"path": "a.txt", "old_string": "alpha", "new_string": "beta"}),
        &context,
    )
    .await;

    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "beta\n"
    );
}

#[tokio::test]
async fn a_successful_edit_leaves_the_file_editable_again() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
    let context = ctx(dir.path());
    read_first(&registry(), "a.txt", &context).await;
    let first = run(
        &registry(),
        "edit",
        serde_json::json!({"path": "a.txt", "old_string": "alpha", "new_string": "beta"}),
        &context,
    )
    .await;
    assert!(!first.is_error, "{}", first.content);

    let second = run(
        &registry(),
        "edit",
        serde_json::json!({"path": "a.txt", "old_string": "beta", "new_string": "gamma"}),
        &context,
    )
    .await;

    assert!(!second.is_error, "{}", second.content);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "gamma\n"
    );
}

/// Two contexts, two models: what one has seen tells the other nothing.
#[tokio::test]
async fn one_contexts_read_does_not_unlock_anothers_edit() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
    let reader = ctx(dir.path());
    read_first(&registry(), "a.txt", &reader).await;

    let out = run(
        &registry(),
        "edit",
        serde_json::json!({"path": "a.txt", "old_string": "alpha", "new_string": "beta"}),
        &ctx(dir.path()),
    )
    .await;

    assert!(out.is_error, "{}", out.content);
    assert!(
        out.content.contains("you have not read this file"),
        "{}",
        out.content
    );
}

// ---- edit: no-match diagnostics ----

#[tokio::test]
async fn edit_no_match_shows_the_closest_region_with_line_numbers() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.txt"),
        "fn main() {\n    let total = compute(2);\n    println!(\"{total}\");\n}\n",
    )
    .unwrap();
    let context = ctx(dir.path());
    read_first(&registry(), "a.txt", &context).await;

    let out = run(
        &registry(),
        "edit",
        serde_json::json!({
            "path": "a.txt",
            // Drifted: the model remembers compute(1).
            "old_string": "    let total = compute(1);",
            "new_string": "    let total = compute(3);"
        }),
        &context,
    )
    .await;

    assert!(out.is_error, "{}", out.content);
    assert!(
        out.content.contains("2→    let total = compute(2);"),
        "{}",
        out.content
    );
    assert!(out.content.contains("line 2"), "{}", out.content);
}

#[tokio::test]
async fn edit_no_match_says_when_nothing_in_the_file_is_close() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
    let context = ctx(dir.path());
    read_first(&registry(), "a.txt", &context).await;

    let out = run(
        &registry(),
        "edit",
        serde_json::json!({
            "path": "a.txt",
            "old_string": "zzzzzzzzzzzzzzzzzzzzz",
            "new_string": "y"
        }),
        &context,
    )
    .await;

    assert!(out.is_error, "{}", out.content);
    assert!(
        out.content.contains("nothing in the file is close to it"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn edit_names_old_string_when_it_carries_read_line_numbers() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\nbeta\n").unwrap();
    let context = ctx(dir.path());
    read_first(&registry(), "a.txt", &context).await;

    let out = run(
        &registry(),
        "edit",
        serde_json::json!({
            "path": "a.txt",
            "old_string": "1→alpha\n2→beta",
            "new_string": "alpha\nBETA"
        }),
        &context,
    )
    .await;

    assert!(out.is_error, "{}", out.content);
    assert!(out.content.contains("old_string"), "{}", out.content);
    assert!(!out.content.contains("new_string"), "{}", out.content);
    assert!(out.content.contains("N→"), "{}", out.content);
}

/// The destructive case: old_string matches, so without this the edit
/// would happily write read's line numbers into the file.
#[tokio::test]
async fn edit_refuses_a_new_string_that_pasted_read_output() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\nbeta\n").unwrap();
    let context = ctx(dir.path());
    read_first(&registry(), "a.txt", &context).await;

    let out = run(
        &registry(),
        "edit",
        serde_json::json!({
            "path": "a.txt",
            "old_string": "alpha\nbeta",
            "new_string": "1→ALPHA\n2→BETA"
        }),
        &context,
    )
    .await;

    assert!(out.is_error, "{}", out.content);
    assert!(
        out.content
            .contains("the file would end up containing them"),
        "{}",
        out.content
    );
    assert!(out.content.contains("use write"), "{}", out.content);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "alpha\nbeta\n"
    );
}

/// One `12→…` line is something a document may legitimately quote, so
/// the refusal wants the two-consecutive-numbers signature of pasted
/// read output before it fires.
#[tokio::test]
async fn a_single_line_number_in_new_string_still_edits() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.md"), "quote:\nPLACEHOLDER\n").unwrap();
    let context = ctx(dir.path());
    read_first(&registry(), "a.md", &context).await;

    let out = run(
        &registry(),
        "edit",
        serde_json::json!({
            "path": "a.md",
            "old_string": "PLACEHOLDER",
            "new_string": "12→    let total = compute(2);"
        }),
        &context,
    )
    .await;

    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.md")).unwrap(),
        "quote:\n12→    let total = compute(2);\n"
    );
}

/// A file that really does contain read output: old_string was copied
/// out of it and carries the prefixes too, which is what tells the
/// asymmetric rule this edit is the genuine article.
#[tokio::test]
async fn symmetric_line_numbers_edit_a_file_that_really_contains_them() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.md"),
        "read output:\n12→fn main() {\n13→    work();\n14→}\n",
    )
    .unwrap();
    let context = ctx(dir.path());
    read_first(&registry(), "a.md", &context).await;

    let out = run(
        &registry(),
        "edit",
        serde_json::json!({
            "path": "a.md",
            "old_string": "12→fn main() {\n13→    work();",
            "new_string": "12→fn main() {\n13→    rest();"
        }),
        &context,
    )
    .await;

    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.md")).unwrap(),
        "read output:\n12→fn main() {\n13→    rest();\n14→}\n"
    );
}

#[tokio::test]
async fn edit_names_new_string_when_it_carries_read_line_numbers() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\nbeta\n").unwrap();
    let context = ctx(dir.path());
    read_first(&registry(), "a.txt", &context).await;

    let out = run(
        &registry(),
        "edit",
        serde_json::json!({
            "path": "a.txt",
            "old_string": "alpha\ndelta",
            "new_string": "1→alpha\n2→BETA"
        }),
        &context,
    )
    .await;

    assert!(out.is_error, "{}", out.content);
    assert!(out.content.contains("new_string"), "{}", out.content);
    assert!(!out.content.contains("old_string"), "{}", out.content);
    assert!(out.content.contains("N→"), "{}", out.content);
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
    // 30KB preview by design, which is fine).
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
        out.content.len() >= 25_000,
        "output suspiciously small: {}",
        out.content.len()
    );
    assert!(out.content.len() < 40_000, "output was not bounded");
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
    assert!(out.content.len() < 40_000, "output was not bounded");
}

#[tokio::test]
async fn bash_keeps_stderr_and_the_stdout_tail_when_stdout_fills_the_preview() {
    // The failure of a chatty build lives in the last stdout lines and in
    // stderr; keeping the first 30KB of the concatenation loses both.
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
    assert!(out.content.len() < 40_000, "output was not bounded");
    // 300000 + "stdout-tail\n" (12) + "fatal: the real error\n" (22)
    assert!(
        out.content.contains("from 300034 raw bytes"),
        "raw byte total misreported: {}",
        &out.content[out.content.len().saturating_sub(400)..]
    );
    // Nowhere to spill to: the preview is still the whole answer.
    assert!(
        !out.content.contains("full output:"),
        "{}",
        &out.content[out.content.len().saturating_sub(400)..]
    );
}

/// The spill workflow end to end: the model gets a small tail preview
/// with stderr intact and a path it can grep for everything else.
#[tokio::test]
async fn bash_spills_output_past_the_preview_and_names_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let spill_dir = state.path().join("tool-output");
    let out = run(
        &registry(),
        "bash",
        serde_json::json!({
            "command": "yes 0123456789 | head -c 300000; printf 'stdout-tail\\n'; \
                        printf 'fatal: the real error\\n' >&2; exit 1",
            "timeout_ms": 10000
        }),
        &spilling_ctx(dir.path(), &spill_dir, "call-spill-1"),
    )
    .await;

    assert!(out.is_error, "{}", out.content);
    assert!(out.content.len() < 40_000, "preview was not bounded");
    assert!(
        out.content.contains("fatal: the real error"),
        "stderr was dropped: {}",
        out.content
    );
    assert!(
        out.content.contains("stdout-tail"),
        "stdout tail was dropped: {}",
        out.content
    );

    let path = hinted_spill_path(&out.content);
    assert_eq!(path.parent(), Some(spill_dir.as_path()), "{path:?}");
    // Session-qualified: a provider call id is unique within a response,
    // not across the sessions sharing this directory.
    assert_eq!(path.file_name().unwrap(), "session-1-call-spill-1.txt");
    // The hint leads, the preview follows, the truncation note closes it.
    let after_hint = out
        .content
        .split_once('\n')
        .expect("the result is more than its hint")
        .1;
    assert!(after_hint.contains("stdout-tail"), "{after_hint}");
    assert!(
        after_hint.contains("fatal: the real error"),
        "stderr moved above the hint"
    );
    assert!(
        out.content.rfind("output truncated at") > out.content.find("stdout-tail"),
        "the truncation note left the end of the preview"
    );
    let spilled = std::fs::read_to_string(&path).expect("the hinted file exists");
    assert!(spilled.contains("=== stdout ==="), "{}", &spilled[..64]);
    assert!(spilled.contains("=== stderr ==="));
    assert!(spilled.contains("fatal: the real error"));
    assert!(spilled.contains("stdout-tail"));
    assert!(
        spilled.len() > 300_000,
        "the file holds a preview, not the capture: {}",
        spilled.len()
    );
    // Nothing was dropped on the way to disk, so no raw-total caveat.
    assert!(!out.content.contains("holds the last"), "{}", out.content);
}

#[tokio::test]
async fn bash_output_within_the_preview_spills_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let spill_dir = state.path().join("tool-output");
    let out = run(
        &registry(),
        "bash",
        serde_json::json!({"command": "echo small-enough"}),
        &spilling_ctx(dir.path(), &spill_dir, "call-spill-2"),
    )
    .await;

    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("small-enough"));
    assert!(!out.content.contains("full output:"), "{}", out.content);
    assert!(!spill_dir.exists(), "a small result created a spill file");
}

/// Even the 2 MiB capture has an end. What survives is the tail, and the
/// hint says so instead of implying the file holds everything.
#[tokio::test]
async fn bash_spill_reports_raw_totals_when_the_capture_itself_truncated() {
    let dir = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let spill_dir = state.path().join("tool-output");
    let out = run(
        &registry(),
        "bash",
        serde_json::json!({
            "command": "yes 0123456789 | head -c 3000000; printf 'the-very-end\\n'",
            "timeout_ms": 20000
        }),
        &spilling_ctx(dir.path(), &spill_dir, "call-spill-3"),
    )
    .await;

    assert!(!out.is_error, "{}", out.content);
    let path = hinted_spill_path(&out.content);
    let spilled = std::fs::read(&path).expect("the hinted file exists");
    assert_eq!(spilled.len(), 2 * 1024 * 1024, "capture cap moved");
    assert!(
        spilled.ends_with(b"the-very-end\n"),
        "the capture kept the head, not the tail"
    );
    // Second line, right under the hint it qualifies.
    assert!(
        out.content
            .lines()
            .nth(1)
            .is_some_and(|line| line.contains("holds the last 2.0 MiB of 3000013 raw bytes")),
        "raw totals misreported: {}",
        &out.content[..out.content.len().min(400)]
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

/// Spilled tool output lives in the state dir, outside any workspace:
/// an absolute pattern has to reach it, and report absolute paths.
#[tokio::test]
async fn glob_accepts_absolute_patterns_outside_cwd() {
    let cwd = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let outside = std::fs::canonicalize(elsewhere.path()).unwrap();
    std::fs::write(outside.join("call-1.txt"), "spilled\n").unwrap();
    std::fs::write(outside.join("other.md"), "").unwrap();

    let out = run(
        &registry(),
        "glob",
        serde_json::json!({ "pattern": format!("{}/*.txt", outside.display()) }),
        &ctx(cwd.path()),
    )
    .await;

    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        out.content,
        outside.join("call-1.txt").to_string_lossy(),
        "{}",
        out.content
    );
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

/// The workflow the spill hint asks for: grep a file the model was
/// pointed at by absolute path, from a cwd that does not contain it.
#[tokio::test]
async fn grep_accepts_absolute_paths_outside_cwd() {
    let cwd = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let outside = std::fs::canonicalize(elsewhere.path()).unwrap();
    let spill = outside.join("call-1.txt");
    std::fs::write(&spill, "noise\nthe needle\n").unwrap();

    for path in [spill.clone(), outside.clone()] {
        let out = run(
            &registry(),
            "grep",
            serde_json::json!({"pattern": "needle", "path": path.to_string_lossy()}),
            &ctx(cwd.path()),
        )
        .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content
                .contains(&format!("{}:2:the needle", spill.display())),
            "searching {}: {}",
            path.display(),
            out.content
        );
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

#[test]
fn the_allowlist_is_derived_from_what_the_constructors_register() {
    use ilar::tools::ChildTool;

    let known = ilar::tools::child_tool_names();
    // The builtins are the builtin registry's own tools, in its order.
    assert_eq!(
        &known[..ToolRegistry::builtin().tool_names().len()],
        ToolRegistry::builtin().tool_names()
    );
    // The rest is the child-tool table, and nothing else.
    assert_eq!(
        &known[ToolRegistry::builtin().tool_names().len()..],
        ChildTool::ALL
            .iter()
            .map(|tool| tool.name())
            .collect::<Vec<_>>()
    );

    // Each table entry is the name its constructor actually installs.
    let dir = tempfile::tempdir().unwrap();
    let installed = |registry: ToolRegistry| {
        let builtin = ToolRegistry::builtin().tool_names();
        registry
            .tool_names()
            .into_iter()
            .filter(|name| !builtin.contains(name))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        installed(ToolRegistry::builtin().with_models(Vec::new()).unwrap()),
        [ChildTool::MODELS.name()]
    );
    assert_eq!(
        installed(
            ToolRegistry::builtin()
                .with_history(ilar::session::SessionStore::new(dir.path().to_path_buf()))
                .unwrap()
        ),
        [ChildTool::HISTORY.name()]
    );
    assert_eq!(
        installed(
            ToolRegistry::builtin()
                .with_services(ilar::tools::service::ServiceManager::new())
                .unwrap()
        ),
        [ChildTool::SERVICE.name()]
    );
    // task/tasks need a live spawner; their registration is checked by
    // the assertion inside `with_child_tool`, which every subagent test
    // exercises.
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
                cwd: None,
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
