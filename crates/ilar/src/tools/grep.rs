//! grep: regex search across files, gitignore-aware, file:line:match.

use serde::Deserialize;
use std::io::{BufRead, Read as _};
use std::sync::atomic::Ordering;

use super::{
    Tool, ToolConcurrency, ToolContext, ToolFuture, ToolOutput, WorkspaceAccess, parse_input,
};

const MAX_MATCHES: usize = 200;
const MAX_MATCHES_PER_FILE: usize = 50;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_OUTPUT_LINE_BYTES: usize = 8 * 1024;
/// Files visited before the walk gives up. Bounds a rare-match search
/// over a monorepo, which the match cap cannot short-circuit.
const MAX_ENTRIES: usize = 500_000;
const MAX_THREADS: usize = 8;

pub struct GrepTool;

#[derive(Deserialize)]
struct Input {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    /// Search gitignored files too. Off by default.
    #[serde(default)]
    include_ignored: bool,
}

/// One rendered `path:line:text` hit, kept with its sort key so parallel
/// walking cannot reorder the output.
struct Hit {
    path: String,
    line: usize,
    rendered: String,
}

impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> &'static str {
        "Search file contents with a regex, recursively from cwd (or path). \
         Gitignored files are skipped unless include_ignored is set. \
         Returns file:line:match."
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
                "pattern": {"type": "string", "description": "Rust regex"},
                "path": {"type": "string", "description": "Subdirectory to search (default: cwd)"},
                "include_ignored": {
                    "type": "boolean",
                    "description": "Search gitignored files too (default false)"
                }
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
            let relative = input.path.as_deref().unwrap_or(".");
            if let Err(error) = super::ensure_workspace_relative(relative, "grep") {
                return error;
            }
            let root = ctx.cwd.join(relative);
            match super::blocking_scan(move |cancelled| {
                grep_files(
                    &ctx.cwd,
                    &root,
                    &input.pattern,
                    input.include_ignored,
                    &cancelled,
                )
            })
            .await
            {
                Ok(output) => output,
                Err(error) => ToolOutput::error(format!("grep worker failed: {error}")),
            }
        })
    }
}

/// Scan one file. Pure apart from reading it: returns the hits and
/// whether the file's byte cap clipped the scan.
fn grep_one_file(
    path: &std::path::Path,
    relative: &str,
    regex: &regex::Regex,
    cancelled: &std::sync::atomic::AtomicBool,
) -> (Vec<Hit>, bool) {
    let Ok(file) = std::fs::File::open(path) else {
        return (Vec::new(), false);
    };
    let mut reader = std::io::BufReader::new(file).take(MAX_FILE_BYTES + 1);
    let mut hits = Vec::new();
    let mut truncated = false;
    let mut line = Vec::new();
    let mut line_number = 0_usize;
    let mut file_bytes = 0_u64;
    loop {
        if cancelled.load(Ordering::Acquire) {
            return (hits, truncated);
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
            let mut rendered = format!("{relative}:{line_number}:{}", text.trim_end());
            truncate_utf8(&mut rendered, MAX_OUTPUT_LINE_BYTES);
            hits.push(Hit {
                path: relative.to_string(),
                line: line_number,
                rendered,
            });
            if hits.len() >= MAX_MATCHES_PER_FILE {
                truncated = true;
                break;
            }
        }
        if file_limit_reached {
            break;
        }
    }
    (hits, truncated)
}

fn grep_files(
    cwd: &std::path::Path,
    root: &std::path::Path,
    pattern: &str,
    include_ignored: bool,
    cancelled: &std::sync::atomic::AtomicBool,
) -> ToolOutput {
    let regex = match regex::Regex::new(pattern) {
        Ok(regex) => regex,
        Err(error) => return ToolOutput::error(format!("grep: invalid regex: {error}")),
    };
    let threads = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
        .min(MAX_THREADS);
    // Matches glob: dotted paths stay searchable (`.github/**`, `.env`),
    // ignore files are honoured even outside a git repo, `.git` is out.
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .ignore(!include_ignored)
        .git_ignore(!include_ignored)
        .git_global(!include_ignored)
        .git_exclude(!include_ignored)
        .parents(!include_ignored)
        .require_git(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .threads(threads)
        .build_parallel();

    let hits = std::sync::Mutex::new(Vec::<Hit>::new());
    let scanned = std::sync::atomic::AtomicUsize::new(0);
    let clipped = std::sync::atomic::AtomicBool::new(false);
    let capped_entries = std::sync::atomic::AtomicBool::new(false);

    walker.run(|| {
        Box::new(|entry| {
            if cancelled.load(Ordering::Acquire) {
                return ignore::WalkState::Quit;
            }
            let Ok(entry) = entry else {
                return ignore::WalkState::Continue;
            };
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                return ignore::WalkState::Continue;
            }
            if scanned.fetch_add(1, Ordering::Relaxed) >= MAX_ENTRIES {
                capped_entries.store(true, Ordering::Release);
                return ignore::WalkState::Quit;
            }
            if hits.lock().unwrap().len() >= MAX_MATCHES {
                return ignore::WalkState::Quit;
            }
            let relative = entry
                .path()
                .strip_prefix(cwd)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .into_owned();
            let (found, file_clipped) = grep_one_file(entry.path(), &relative, &regex, cancelled);
            if file_clipped {
                clipped.store(true, Ordering::Release);
            }
            if !found.is_empty() {
                hits.lock().unwrap().extend(found);
            }
            ignore::WalkState::Continue
        })
    });

    if cancelled.load(Ordering::Acquire) {
        return ToolOutput::error("cancelled");
    }
    let mut hits = hits.into_inner().unwrap();
    // Parallel walking loses walk order; make the output reproducible.
    hits.sort_by(|left, right| left.path.cmp(&right.path).then(left.line.cmp(&right.line)));
    let mut truncated = clipped.load(Ordering::Acquire);
    if hits.len() > MAX_MATCHES {
        hits.truncate(MAX_MATCHES);
        truncated = true;
    }
    let mut out = String::new();
    for hit in hits {
        if out.len().saturating_add(hit.rendered.len() + 1) > MAX_OUTPUT_BYTES {
            truncated = true;
            break;
        }
        out.push_str(&hit.rendered);
        out.push('\n');
    }
    if capped_entries.load(Ordering::Acquire) {
        out.push_str(&format!(
            "…(truncated: scanned {MAX_ENTRIES} files without finishing; narrow the path)\n"
        ));
    } else if truncated {
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
