//! glob: file pattern matching (e.g. src/**/*.rs).

use serde::Deserialize;

use super::{Tool, ToolContext, ToolFuture, ToolKind, ToolOutput, parse_input};

const MAX_MATCHES: usize = 1000;

pub struct GlobTool;

#[derive(Deserialize)]
struct Input {
    pattern: String,
}

impl Tool for GlobTool {
    fn name(&self) -> &'static str {
        "glob"
    }

    fn description(&self) -> &'static str {
        "Find files by glob pattern (e.g. src/**/*.rs), relative to cwd."
    }

    fn kind(&self) -> ToolKind {
        ToolKind::ReadOnly
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string"}
            },
            "required": ["pattern"]
        })
    }

    fn run(&self, input: serde_json::Value, ctx: ToolContext) -> ToolFuture {
        Box::pin(async move {
            let input: Input = match parse_input(input, "glob") {
                Ok(v) => v,
                Err(e) => return e,
            };
            let pattern = ctx.cwd.join(&input.pattern);
            let pattern = pattern.to_string_lossy().to_string();
            let globber = match glob::glob(&pattern) {
                Ok(g) => g,
                Err(e) => return ToolOutput::error(format!("glob: invalid pattern: {e}")),
            };
            let mut matches: Vec<String> = globber
                .flatten()
                .filter_map(|p| {
                    p.strip_prefix(&ctx.cwd)
                        .ok()
                        .map(|r| r.to_string_lossy().into_owned())
                })
                .collect();
            matches.sort();
            if matches.len() > MAX_MATCHES {
                matches.truncate(MAX_MATCHES);
                matches.push(format!("…(truncated at {MAX_MATCHES} matches)"));
            }
            if matches.is_empty() {
                ToolOutput::text("(no matches)")
            } else {
                ToolOutput::text(matches.join("\n"))
            }
        })
    }
}
