//! glob: file pattern matching (e.g. src/**/*.rs).

use serde::Deserialize;
use std::sync::atomic::Ordering;

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
            match super::blocking_scan(move |cancelled| {
                let pattern = match glob::Pattern::new(&input.pattern) {
                    Ok(pattern) => pattern,
                    Err(error) => {
                        return ToolOutput::error(format!("glob: invalid pattern: {error}"));
                    }
                };
                let options = glob::MatchOptions {
                    case_sensitive: true,
                    require_literal_separator: true,
                    require_literal_leading_dot: false,
                };
                let walker = ignore::WalkBuilder::new(&ctx.cwd)
                    .hidden(false)
                    .ignore(false)
                    .git_ignore(false)
                    .git_global(false)
                    .git_exclude(false)
                    .parents(false)
                    .sort_by_file_name(|left, right| left.cmp(right))
                    .build();
                let mut matches = Vec::new();
                let mut truncated = false;
                for entry in walker {
                    if cancelled.load(Ordering::Acquire) {
                        return ToolOutput::error("cancelled");
                    }
                    let Ok(entry) = entry else {
                        continue;
                    };
                    let path = entry.path();
                    let Ok(relative) = path.strip_prefix(&ctx.cwd) else {
                        continue;
                    };
                    if relative.as_os_str().is_empty()
                        || !pattern.matches_path_with(relative, options)
                    {
                        continue;
                    }
                    if matches.len() == MAX_MATCHES {
                        truncated = true;
                        break;
                    }
                    matches.push(relative.to_string_lossy().into_owned());
                }
                matches.sort();
                if truncated {
                    matches.push(format!("…(truncated at {MAX_MATCHES} matches)"));
                }
                if matches.is_empty() {
                    ToolOutput::text("(no matches)")
                } else {
                    ToolOutput::text(matches.join("\n"))
                }
            })
            .await
            {
                Ok(output) => output,
                Err(error) => ToolOutput::error(format!("glob worker failed: {error}")),
            }
        })
    }
}
