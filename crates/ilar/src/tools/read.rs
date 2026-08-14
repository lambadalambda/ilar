//! read: file contents with line numbers, offset/limit windowing,
//! size caps.

use serde::Deserialize;

use super::{Tool, ToolContext, ToolFuture, ToolKind, ToolOutput, parse_input};

const MAX_LINES: usize = 2000;
const MAX_BYTES: usize = 256 * 1024;

pub struct ReadTool;

#[derive(Deserialize)]
struct Input {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "read"
    }

    fn description(&self) -> &'static str {
        "Read a text file. Returns numbered lines (N→line). Use offset/limit \
         for large files."
    }

    fn kind(&self) -> ToolKind {
        ToolKind::ReadOnly
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path, relative to cwd"},
                "offset": {"type": "integer", "description": "1-based line to start at"},
                "limit": {"type": "integer", "description": "Max lines to return"}
            },
            "required": ["path"]
        })
    }

    fn run(&self, input: serde_json::Value, ctx: ToolContext) -> ToolFuture {
        Box::pin(async move {
            let input: Input = match parse_input(input, "read") {
                Ok(v) => v,
                Err(e) => return e,
            };
            let path = ctx.cwd.join(&input.path);
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(e) => return ToolOutput::error(format!("read {}: {e}", input.path)),
            };
            if bytes.len() > MAX_BYTES {
                return ToolOutput::error(format!(
                    "read {}: file is {} bytes, over the {} byte limit; use offset/limit after narrowing with grep",
                    input.path,
                    bytes.len(),
                    MAX_BYTES
                ));
            }
            let text = String::from_utf8_lossy(&bytes);
            let start = input.offset.unwrap_or(1).max(1);
            let limit = input.limit.unwrap_or(MAX_LINES).min(MAX_LINES);
            let mut out = String::new();
            let mut count = 0usize;
            for (i, line) in text.lines().enumerate() {
                let n = i + 1;
                if n < start {
                    continue;
                }
                if count >= limit {
                    out.push_str("…\n(truncated)\n");
                    break;
                }
                out.push_str(&format!("{n}→{line}\n"));
                count += 1;
            }
            if out.is_empty() {
                out = format!("(empty file: {})", input.path);
            }
            ToolOutput::text(out)
        })
    }
}
