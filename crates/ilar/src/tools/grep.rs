//! grep: regex search across files, gitignore-aware, file:line:match.

use serde::Deserialize;

use super::{Tool, ToolContext, ToolFuture, ToolKind, ToolOutput, parse_input};

const MAX_MATCHES: usize = 200;

pub struct GrepTool;

#[derive(Deserialize)]
struct Input {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
}

impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> &'static str {
        "Search file contents with a regex, recursively from cwd (or path). \
         Gitignored files are skipped. Returns file:line:match."
    }

    fn kind(&self) -> ToolKind {
        ToolKind::ReadOnly
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Rust regex"},
                "path": {"type": "string", "description": "Subdirectory to search (default: cwd)"}
            },
            "required": ["pattern"]
        })
    }

    fn run(&self, input: serde_json::Value, ctx: ToolContext) -> ToolFuture {
        Box::pin(async move {
            let input: Input = match parse_input(input, "grep") {
                Ok(v) => v,
                Err(e) => return e,
            };
            let regex = match regex::Regex::new(&input.pattern) {
                Ok(r) => r,
                Err(e) => return ToolOutput::error(format!("grep: invalid regex: {e}")),
            };
            let root = ctx.cwd.join(input.path.as_deref().unwrap_or("."));
            let mut out = String::new();
            let mut count = 0usize;
            let walker = ignore::WalkBuilder::new(&root)
                .hidden(true)
                .git_ignore(true)
                .build();
            for entry in walker.flatten() {
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    continue;
                }
                if count >= MAX_MATCHES {
                    out.push_str("…(truncated)\n");
                    break;
                }
                let Ok(content) = std::fs::read_to_string(entry.path()) else {
                    continue;
                };
                for (i, line) in content.lines().enumerate() {
                    if count >= MAX_MATCHES {
                        break;
                    }
                    if regex.is_match(line) {
                        let rel = entry
                            .path()
                            .strip_prefix(&ctx.cwd)
                            .unwrap_or(entry.path())
                            .to_string_lossy();
                        out.push_str(&format!("{rel}:{}:{}\n", i + 1, line.trim_end()));
                        count += 1;
                    }
                }
            }
            ToolOutput::text(out)
        })
    }
}
