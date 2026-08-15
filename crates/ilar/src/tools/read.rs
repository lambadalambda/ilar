//! read: file contents with line numbers, offset/limit windowing,
//! size caps.

use serde::Deserialize;
use std::fmt::Write as _;
use std::io::BufRead;
use std::sync::atomic::Ordering;

use super::{
    Tool, ToolConcurrency, ToolContext, ToolFuture, ToolOutput, WorkspaceAccess, parse_input,
};

const MAX_LINES: usize = 2000;
const MAX_OUTPUT_BYTES: usize = 256 * 1024;

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

    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Concurrent
    }
    fn workspace_access(&self) -> WorkspaceAccess {
        WorkspaceAccess::ReadOnly
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
            let start = input.offset.unwrap_or(1).max(1);
            let limit = input.limit.unwrap_or(MAX_LINES).min(MAX_LINES);
            match super::blocking_scan(move |cancelled| {
                read_window(&path, &input.path, start, limit, &cancelled)
            })
            .await
            {
                Ok(output) => output,
                Err(error) => ToolOutput::error(format!("read worker failed: {error}")),
            }
        })
    }
}

fn read_window(
    path: &std::path::Path,
    display_path: &str,
    start: usize,
    limit: usize,
    cancelled: &std::sync::atomic::AtomicBool,
) -> ToolOutput {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) => return ToolOutput::error(format!("read {display_path}: {error}")),
    };
    let mut reader = std::io::BufReader::new(file);
    let mut line_number = 0_usize;
    let mut emitted = 0_usize;
    let mut out = String::new();
    let mut reached_eof = false;
    let mut truncated = false;

    loop {
        if cancelled.load(Ordering::Acquire) {
            return ToolOutput::error("cancelled");
        }
        let next_number = line_number + 1;
        let selected = next_number >= start && emitted < limit;
        let prefix_overhead = next_number.to_string().len() + "→\n".len();
        let keep = if selected {
            MAX_OUTPUT_BYTES
                .saturating_sub(out.len())
                .saturating_sub(prefix_overhead)
        } else {
            0
        };
        let line = match read_line_prefix(&mut reader, keep, cancelled) {
            Ok(Some(line)) => line,
            Ok(None) => {
                reached_eof = true;
                break;
            }
            Err(error) => return ToolOutput::error(format!("read {display_path}: {error}")),
        };
        line_number = next_number;
        if line_number < start {
            continue;
        }
        if emitted >= limit {
            truncated = true;
            break;
        }
        let mut text = String::from_utf8_lossy(&line.prefix).into_owned();
        truncate_utf8(&mut text, keep);
        let _ = writeln!(out, "{line_number}→{}", text.trim_end_matches('\r'));
        emitted += 1;
        if line.truncated || out.len() >= MAX_OUTPUT_BYTES {
            truncated = true;
            break;
        }
    }

    if line_number == 0 && reached_eof {
        return ToolOutput::text(format!("(empty file: {display_path})"));
    }
    if emitted == 0 && reached_eof && start > line_number {
        return ToolOutput::error(format!(
            "read {display_path}: offset {start} is beyond end of file ({line_number} lines)"
        ));
    }
    if truncated {
        const MARKER: &str = "…\n(truncated)\n";
        truncate_utf8(&mut out, MAX_OUTPUT_BYTES.saturating_sub(MARKER.len()));
        out.push_str(MARKER);
    }
    ToolOutput::text(out)
}

fn truncate_utf8(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
}

struct LinePrefix {
    prefix: Vec<u8>,
    truncated: bool,
}

fn read_line_prefix<R: BufRead>(
    reader: &mut R,
    keep: usize,
    cancelled: &std::sync::atomic::AtomicBool,
) -> std::io::Result<Option<LinePrefix>> {
    let mut prefix = Vec::new();
    let mut saw_bytes = false;
    let mut truncated = false;
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "cancelled",
            ));
        }
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(saw_bytes.then_some(LinePrefix { prefix, truncated }));
        }
        saw_bytes = true;
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |index| index + 1);
        let content = newline.map_or(&available[..consumed], |index| &available[..index]);
        let retained = content.len().min(keep.saturating_sub(prefix.len()));
        prefix.extend_from_slice(&content[..retained]);
        truncated |= retained < content.len();
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(Some(LinePrefix { prefix, truncated }));
        }
    }
}
