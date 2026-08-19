//! glob: file pattern matching (e.g. src/**/*.rs).

use serde::Deserialize;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::{
    Tool, ToolConcurrency, ToolContext, ToolFuture, ToolOutput, WorkspaceAccess, parse_input,
};

const MAX_MATCHES: usize = 1000;
/// Entries visited before the walk gives up. Bounds the pathological
/// case (an unfiltered monorepo) to seconds instead of an apparent hang.
const MAX_ENTRIES: usize = 500_000;
const MAX_THREADS: usize = 8;

pub struct GlobTool;

#[derive(Deserialize)]
struct Input {
    pattern: String,
    /// Include gitignored paths (build output, `.env`). Off by default.
    #[serde(default)]
    include_ignored: bool,
}

#[derive(Clone, Copy)]
struct Limits {
    max_matches: usize,
    max_entries: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_matches: MAX_MATCHES,
            max_entries: MAX_ENTRIES,
        }
    }
}

/// Leading path components that contain no glob metacharacters. Walking
/// can start there instead of the workspace root, which is the whole
/// difference between opening one directory and enumerating a monorepo.
///
/// Errors when the pattern is absolute or contains `..`, which would
/// walk outside the workspace.
fn literal_prefix(pattern: &str) -> Result<PathBuf, String> {
    let path = Path::new(pattern);
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            _ => {
                return Err(
                    "glob: pattern must stay within the workspace (no leading / or ..)".into(),
                );
            }
        }
    }
    let mut prefix = PathBuf::new();
    for component in path.components() {
        let Component::Normal(part) = component else {
            continue;
        };
        if part.to_string_lossy().contains(['*', '?', '[', ']']) {
            break;
        }
        prefix.push(part);
    }
    Ok(prefix)
}

fn scan(
    cwd: &Path,
    pattern: &str,
    include_ignored: bool,
    limits: Limits,
    cancelled: &AtomicBool,
) -> ToolOutput {
    let compiled = match glob::Pattern::new(pattern) {
        Ok(compiled) => compiled,
        Err(error) => return ToolOutput::error(format!("glob: invalid pattern: {error}")),
    };
    let prefix = match literal_prefix(pattern) {
        Ok(prefix) => prefix,
        Err(error) => return ToolOutput::error(error),
    };
    let options = glob::MatchOptions {
        case_sensitive: true,
        require_literal_separator: true,
        require_literal_leading_dot: false,
    };

    let root = cwd.join(&prefix);
    let threads = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
        .min(MAX_THREADS);
    // Hidden entries stay visible so `.github/workflows/*.yml` keeps
    // working; the ignore files do the heavy filtering, and `.git` is
    // dropped explicitly because nothing there is a source file.
    let walker = ignore::WalkBuilder::new(&root)
        .hidden(false)
        .ignore(!include_ignored)
        .git_ignore(!include_ignored)
        .git_global(!include_ignored)
        .git_exclude(!include_ignored)
        .parents(!include_ignored)
        // Honour ignore files even outside a git repository.
        .require_git(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .threads(threads)
        .build_parallel();

    let matches = Mutex::new(Vec::new());
    let scanned = AtomicUsize::new(0);
    let capped_matches = AtomicBool::new(false);
    let capped_entries = AtomicBool::new(false);

    walker.run(|| {
        Box::new(|entry| {
            if cancelled.load(Ordering::Acquire) {
                return ignore::WalkState::Quit;
            }
            if scanned.fetch_add(1, Ordering::Relaxed) >= limits.max_entries {
                capped_entries.store(true, Ordering::Release);
                return ignore::WalkState::Quit;
            }
            let Ok(entry) = entry else {
                return ignore::WalkState::Continue;
            };
            let Ok(relative) = entry.path().strip_prefix(cwd) else {
                return ignore::WalkState::Continue;
            };
            if relative.as_os_str().is_empty() || !compiled.matches_path_with(relative, options) {
                return ignore::WalkState::Continue;
            }
            let mut matches = matches.lock().unwrap();
            if matches.len() >= limits.max_matches {
                capped_matches.store(true, Ordering::Release);
                return ignore::WalkState::Quit;
            }
            matches.push(relative.to_string_lossy().into_owned());
            ignore::WalkState::Continue
        })
    });

    if cancelled.load(Ordering::Acquire) {
        return ToolOutput::error("cancelled");
    }
    let mut matches = matches.into_inner().unwrap();
    matches.sort();
    matches.truncate(limits.max_matches);
    if capped_matches.load(Ordering::Acquire) {
        matches.push(format!("…(truncated at {} matches)", limits.max_matches));
    } else if capped_entries.load(Ordering::Acquire) {
        matches.push(format!(
            "…(truncated: scanned {} paths without finishing; narrow the pattern)",
            limits.max_entries
        ));
    }
    if matches.is_empty() {
        ToolOutput::text("(no matches)")
    } else {
        ToolOutput::text(matches.join("\n"))
    }
}

impl Tool for GlobTool {
    fn name(&self) -> &'static str {
        "glob"
    }

    fn description(&self) -> &'static str {
        "Find files by glob pattern (e.g. src/**/*.rs), relative to cwd. \
         Gitignored paths are skipped unless include_ignored is set."
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
                "pattern": {"type": "string"},
                "include_ignored": {
                    "type": "boolean",
                    "description": "Include gitignored paths (default false)"
                }
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
                scan(
                    &ctx.cwd,
                    &input.pattern,
                    input.include_ignored,
                    Limits::default(),
                    &cancelled,
                )
            })
            .await
            {
                Ok(output) => output,
                Err(error) => ToolOutput::error(format!("glob worker failed: {error}")),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_prefix_stops_at_the_first_metacharacter() {
        assert_eq!(literal_prefix("src/**/*.rs").unwrap(), Path::new("src"));
        assert_eq!(
            literal_prefix("worktrees/manteca/*").unwrap(),
            Path::new("worktrees/manteca")
        );
        assert_eq!(literal_prefix("*.txt").unwrap(), Path::new(""));
        assert_eq!(literal_prefix("**/foo").unwrap(), Path::new(""));
        assert_eq!(literal_prefix("src/a[0-9]/b").unwrap(), Path::new("src"));
        assert_eq!(
            literal_prefix("src/main.rs").unwrap(),
            Path::new("src/main.rs")
        );
        assert_eq!(literal_prefix("./src/*.rs").unwrap(), Path::new("src"));
    }

    #[test]
    fn literal_prefix_rejects_escapes() {
        for pattern in ["../*.rs", "/etc/*", "src/../../*.rs"] {
            assert!(literal_prefix(pattern).is_err(), "{pattern}");
        }
    }

    #[test]
    fn scan_reports_the_entry_budget_separately_from_the_match_cap() {
        let dir = tempfile::tempdir().unwrap();
        for index in 0..20 {
            std::fs::write(dir.path().join(format!("f{index}.txt")), "").unwrap();
        }
        let cancelled = AtomicBool::new(false);
        let out = scan(
            dir.path(),
            "*.txt",
            false,
            Limits {
                max_matches: MAX_MATCHES,
                max_entries: 5,
            },
            &cancelled,
        );
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("scanned 5 paths"), "{}", out.content);
    }
}
