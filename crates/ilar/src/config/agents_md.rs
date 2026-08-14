//! AGENTS.md / CLAUDE.md discovery and system-prompt assembly.

use std::path::{Path, PathBuf};

const BASE_PROMPT: &str = "You are ilar, a terminal coding agent. You have \
tools: read, write, edit, bash, glob, grep. Work in the user's project \
directory. Be terse; verify assumptions against the actual source before \
acting; prefer minimal diffs. When a task is done, stop.";

/// Assemble the system prompt for a working directory: base prompt +
/// nearest AGENTS.md (or CLAUDE.md) content.
pub fn system_prompt_for(cwd: &Path) -> String {
    match find_context_file(cwd) {
        Some((path, content)) => {
            let rel = path
                .strip_prefix(cwd.ancestors().last().unwrap_or(Path::new("/")))
                .unwrap_or(&path);
            let _ = rel;
            format!(
                "{BASE_PROMPT}\n\n# Project context (from {}):\n\n{}",
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("AGENTS.md"),
                content.trim()
            )
        }
        None => BASE_PROMPT.to_string(),
    }
}

/// Nearest AGENTS.md or CLAUDE.md walking up from `cwd`. AGENTS.md beats
/// CLAUDE.md at the same level; closer beats farther.
fn find_context_file(cwd: &Path) -> Option<(PathBuf, String)> {
    let mut cwd = cwd.to_path_buf();
    loop {
        for name in ["AGENTS.md", "CLAUDE.md"] {
            let candidate = cwd.join(name);
            if let Ok(content) = std::fs::read_to_string(&candidate) {
                return Some((candidate, content));
            }
        }
        if !cwd.pop() {
            return None;
        }
    }
}
