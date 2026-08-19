//! AGENTS.md / CLAUDE.md discovery and system-prompt assembly.

use std::path::{Path, PathBuf};

use anyhow::Context;

const BASE_PROMPT: &str = "You are ilar, a terminal coding agent. You have \
tools: read, write, edit, bash, glob, grep. Work in the user's project \
directory. Be terse; verify assumptions against the actual source before \
acting; prefer minimal diffs. When a task is done, stop.";

/// Assemble the system prompt from the user config directory and exact working
/// directory. AGENTS.md wins over CLAUDE.md within each location.
pub fn system_prompt_for(user_config_dir: &Path, cwd: &Path) -> anyhow::Result<String> {
    let mut prompt = BASE_PROMPT.to_string();
    for (label, dir) in [
        ("User context", user_config_dir),
        ("Working directory context", cwd),
    ] {
        if let Some((path, content)) = context_file_in(dir)? {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("AGENTS.md");
            prompt.push_str(&format!(
                "\n\n# {label} (from {name}):\n\n{}",
                content.trim()
            ));
        }
    }
    Ok(prompt)
}

fn context_file_in(dir: &Path) -> anyhow::Result<Option<(PathBuf, String)>> {
    for name in ["AGENTS.md", "CLAUDE.md"] {
        let path = dir.join(name);
        match std::fs::read_to_string(&path) {
            Ok(content) => return Ok(Some((path, content))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("reading context {}", path.display()));
            }
        }
    }
    Ok(None)
}
