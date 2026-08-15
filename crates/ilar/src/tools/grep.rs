//! grep: regex search across files, gitignore-aware, file:line:match.

use serde::Deserialize;
use std::io::{BufRead, Read as _};
use std::sync::atomic::Ordering;

use super::{Tool, ToolContext, ToolFuture, ToolKind, ToolOutput, parse_input};

const MAX_MATCHES: usize = 200;
const MAX_MATCHES_PER_FILE: usize = 50;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_OUTPUT_LINE_BYTES: usize = 8 * 1024;

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
            let root = ctx.cwd.join(input.path.as_deref().unwrap_or("."));
            match super::blocking_scan(move |cancelled| {
                grep_files(&ctx.cwd, &root, &input.pattern, &cancelled)
            })
            .await
            {
                Ok(output) => output,
                Err(error) => ToolOutput::error(format!("grep worker failed: {error}")),
            }
        })
    }
}

fn grep_files(
    cwd: &std::path::Path,
    root: &std::path::Path,
    pattern: &str,
    cancelled: &std::sync::atomic::AtomicBool,
) -> ToolOutput {
    let regex = match regex::Regex::new(pattern) {
        Ok(regex) => regex,
        Err(error) => return ToolOutput::error(format!("grep: invalid regex: {error}")),
    };
    let mut out = String::new();
    let mut count = 0_usize;
    let mut truncated = false;
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .build();
    for entry in walker.flatten() {
        if cancelled.load(Ordering::Acquire) {
            return ToolOutput::error("cancelled");
        }
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if count >= MAX_MATCHES || out.len() >= MAX_OUTPUT_BYTES {
            truncated = true;
            break;
        }
        let Ok(file) = std::fs::File::open(entry.path()) else {
            continue;
        };
        let mut reader = std::io::BufReader::new(file).take(MAX_FILE_BYTES + 1);
        let mut line = Vec::new();
        let mut line_number = 0_usize;
        let mut file_matches = 0_usize;
        let mut file_bytes = 0_u64;
        loop {
            if cancelled.load(Ordering::Acquire) {
                return ToolOutput::error("cancelled");
            }
            line.clear();
            let Ok(read) = reader.read_until(b'\n', &mut line) else {
                break;
            };
            if read == 0 {
                break;
            }
            let previous_file_bytes = file_bytes;
            file_bytes += read as u64;
            let file_limit_reached = file_bytes > MAX_FILE_BYTES;
            if file_limit_reached {
                let remaining = MAX_FILE_BYTES.saturating_sub(previous_file_bytes) as usize;
                truncated = true;
                if remaining == 0 {
                    break;
                }
                line.truncate(remaining);
            }
            if line.last() == Some(&b'\n') {
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
            }
            line_number += 1;
            let text = String::from_utf8_lossy(&line);
            if regex.is_match(&text) {
                let relative = entry
                    .path()
                    .strip_prefix(cwd)
                    .unwrap_or(entry.path())
                    .to_string_lossy();
                let mut rendered = format!("{relative}:{line_number}:{}", text.trim_end());
                truncate_utf8(&mut rendered, MAX_OUTPUT_LINE_BYTES);
                rendered.push('\n');
                if out.len().saturating_add(rendered.len()) > MAX_OUTPUT_BYTES {
                    truncated = true;
                    break;
                }
                out.push_str(&rendered);
                count += 1;
                file_matches += 1;
                if count >= MAX_MATCHES || file_matches >= MAX_MATCHES_PER_FILE {
                    truncated = true;
                    break;
                }
            }
            if file_limit_reached {
                break;
            }
        }
        if truncated && (count >= MAX_MATCHES || out.len() >= MAX_OUTPUT_BYTES) {
            break;
        }
    }
    if truncated {
        const MARKER: &str = "…(truncated)\n";
        truncate_utf8(&mut out, MAX_OUTPUT_BYTES.saturating_sub(MARKER.len()));
        out.push_str(MARKER);
    }
    ToolOutput::text(out)
}

fn truncate_utf8(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes.saturating_sub('…'.len_utf8());
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value.push('…');
}
