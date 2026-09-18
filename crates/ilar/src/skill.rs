//! Skills: markdown + frontmatter, discovered in the user config dir and
//! the project `.ilar/skills/`, loaded on demand via the `skill` tool.
//! The scan happens once per store and keeps names and descriptions;
//! a body is read when it is asked for, and never past
//! [`MAX_SKILL_BYTES`]. Ships the worktree-isolation built-in.

use std::path::PathBuf;

use anyhow::Context;
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// Cue phrases surfaced in the system-prompt listing so the model
    /// invokes the skill when they match the task.
    pub triggers: Vec<String>,
    pub body: String,
}

/// What the listing knows about a skill without holding its body: the
/// prompt line, and where the body is when it is asked for.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    pub triggers: Vec<String>,
    source: SkillSource,
}

#[derive(Debug, Clone, PartialEq)]
enum SkillSource {
    Builtin(&'static str),
    File(PathBuf),
}

/// The most a skill definition may be. A skill is a page of
/// instructions; a file past this is a mistake — a binary, a dump — and
/// would otherwise land in the model's context whole.
pub const MAX_SKILL_BYTES: u64 = 256 * 1024;

/// The built-in worktree-isolation skill: run a subagent in a git worktree.
const WORKTREE_ISOLATION: &str = r#"---
description = "Run a subagent in a separately scheduled git worktree"
---
# Worktree isolation

To run a task in a separately scheduled Git worktree:

1. Create a worktree: `git worktree add ../ilar-task-<name> -b task/<name>`
2. Invoke the `task` tool with structured workspace routing:
   `{"workspace":{"cwd":"../ilar-task-<name>","isolation":"git_worktree"}}`.
   Include the same field when resuming from a different parent workspace.
   Omit it only for a read-only nested task inheriting its immediate
   parent's worktree; a nested mutable task always needs its own.
3. When the subagent finishes, review the diff in the worktree
   (`git -C ../ilar-task-<name> diff`), merge or cherry-pick if good.
4. Clean up: `git worktree remove ../ilar-task-<name>` and delete the
   branch if abandoned.

Use for risky refactors or experiments that should not race concurrent
edits in the main checkout. This is cooperative scheduling, not a sandbox:
tools can still access paths outside the worktree.
"#;

/// The built-in MCP bridge skill: drive MCP servers through an external
/// CLI instead of a core MCP client — see meta/issues/mcp-via-skill.md.
const MCP_VIA_CLI: &str = r#"---
description = "Call MCP server tools through the mcptools CLI (no built-in MCP client)"
triggers = ["MCP server", "model context protocol", "mcp.json", "call an MCP tool"]
---
# MCP via CLI

ilar has no built-in MCP client by design. Reach MCP servers through an
external CLI with the bash tool. Default choice: `mcptools`
(https://github.com/f/mcptools) — install with
`brew install f/mcptools/mcptools` or `go install github.com/f/mcptools/cmd/mcptools@latest`.

1. Discover configured servers. Check, in order: `./.mcp.json`,
   `~/.claude/mcp.json`, `~/.cursor/mcp.json`. Entries follow the common
   `{"mcpServers": {"<name>": {"command": ..., "args": [...], "env": {...}}}}`
   shape (HTTP servers use a `url` field instead).
2. List a server's tools:
   - stdio: `mcp tools <command> <args...>` (e.g. `mcp tools npx -y @modelcontextprotocol/server-filesystem /tmp`)
   - HTTP/SSE: `mcp tools <url>`
3. Call a tool with JSON parameters:
   `mcp call <tool-name> --params '<json>' <command-or-url>`
   Quote the JSON with single quotes; use `--format json` for
   machine-readable output.
4. Set any `env` values from the server entry inline:
   `FOO=bar mcp call ...`.

Notes:
- Each `mcp call` starts a fresh stdio server; that is fine for
  stateless tools. For servers that need a session, use
  `mcp shell <command>` interactively via a background bash job.
- Servers run with whatever access the surrounding sandbox grants; ilar
  adds no credential handling or extra isolation.
- If `mcp` is not installed, say so and show the install commands
  instead of guessing at flags.
"#;

pub fn parse_skill_md(name: &str, text: &str) -> anyhow::Result<Skill> {
    let (frontmatter, body) = crate::config::split_frontmatter(text)?;
    let fm = crate::config::parse_frontmatter(&frontmatter).context("invalid skill frontmatter")?;
    Ok(Skill {
        // A `name` field wins over the filename, so renaming a folder
        // does not silently change how the skill is invoked.
        name: fm.name.unwrap_or_else(|| name.into()),
        description: fm.description.unwrap_or_else(|| name.into()),
        triggers: fm.triggers,
        body: body.trim_start_matches('\n').trim().to_string(),
    })
}

/// Skill files in both layouts: our flat `<name>.md`, and the
/// `<name>/SKILL.md` directory Claude Code and opencode write.
fn skill_files(dir: &std::path::Path) -> anyhow::Result<Vec<(String, PathBuf)>> {
    let mut found: Vec<(String, PathBuf)> = crate::config::markdown_files(dir)?
        .into_iter()
        .filter_map(|path| {
            let name = path.file_stem()?.to_str()?.to_string();
            Some((name, path))
        })
        .collect();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(found),
        Err(error) => {
            return Err(error).with_context(|| format!("reading skills in {}", dir.display()));
        }
    };
    for entry in entries {
        let path = entry
            .with_context(|| format!("reading skills in {}", dir.display()))?
            .path();
        let manifest = path.join("SKILL.md");
        if path.is_dir() && manifest.is_file() {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .with_context(|| format!("skill directory is not UTF-8: {}", path.display()))?;
            found.push((name.to_string(), manifest));
        }
    }
    found.sort();
    Ok(found)
}

pub struct SkillStore {
    user_dir: PathBuf,
    /// `None` for a store that reads the user dir alone.
    project_dir: Option<PathBuf>,
    builtins: bool,
    /// The scan, remembered: the listing is asked for at startup for
    /// the prompt and again for the picker, and the tool asks again for
    /// every load and every unknown name. Each of those used to read
    /// and parse every body. A name the inventory does not know is
    /// looked for again before it is refused — the assistant writes
    /// skills mid-session and loads them at once — but that refresh
    /// reads only files the inventory has not seen; a body is read
    /// fresh whenever its skill is loaded.
    inventory: std::sync::Mutex<Option<Vec<SkillMeta>>>,
}

impl SkillStore {
    pub fn new(user_dir: PathBuf, project_dir: PathBuf) -> Self {
        Self {
            user_dir,
            project_dir: Some(project_dir),
            builtins: true,
            inventory: std::sync::Mutex::new(None),
        }
    }

    /// The user dir's skills and nothing else: no built-ins, no
    /// project `.ilar/skills`.
    pub fn own_only(user_dir: PathBuf) -> Self {
        Self {
            user_dir,
            project_dir: None,
            builtins: false,
            inventory: std::sync::Mutex::new(None),
        }
    }

    /// All available skills: built-ins, user dir, project .ilar/skills
    /// (later wins by name). Names and descriptions, not bodies; the
    /// scan runs once and is remembered.
    pub fn list(&self) -> anyhow::Result<Vec<SkillMeta>> {
        if let Some(inventory) = self.inventory.lock().unwrap().as_ref() {
            return Ok(inventory.clone());
        }
        self.refresh()
    }

    /// Scan again, reusing what the inventory already read: only a file
    /// it has not seen costs a read.
    fn refresh(&self) -> anyhow::Result<Vec<SkillMeta>> {
        let known = self.inventory.lock().unwrap().clone().unwrap_or_default();
        let scanned = self.scan(&known)?;
        *self.inventory.lock().unwrap() = Some(scanned.clone());
        Ok(scanned)
    }

    fn scan(&self, known: &[SkillMeta]) -> anyhow::Result<Vec<SkillMeta>> {
        let mut skills: Vec<SkillMeta> = Vec::new();
        if self.builtins {
            for (name, text) in [
                ("worktree-isolation", WORKTREE_ISOLATION),
                ("mcp-via-cli", MCP_VIA_CLI),
            ] {
                let skill = parse_skill_md(name, text).expect("builtin skill parses");
                skills.push(SkillMeta {
                    name: skill.name,
                    description: skill.description,
                    triggers: skill.triggers,
                    source: SkillSource::Builtin(text),
                });
            }
        }
        let mut dirs = vec![self.user_dir.join("skills")];
        if let Some(project) = &self.project_dir {
            dirs.push(project.join(".ilar/skills"));
        }
        for dir in dirs {
            for (name, path) in skill_files(&dir)? {
                let meta = match known
                    .iter()
                    .find(|meta| meta.source == SkillSource::File(path.clone()))
                {
                    Some(meta) => meta.clone(),
                    None => {
                        // Read once, here, for the frontmatter; the
                        // body is read again only when the skill is
                        // asked for.
                        let skill =
                            parse_skill_md(&name, &read_skill(&path)?).with_context(|| {
                                format!("parsing skill definition {}", path.display())
                            })?;
                        SkillMeta {
                            name: skill.name,
                            description: skill.description,
                            triggers: skill.triggers,
                            source: SkillSource::File(path),
                        }
                    }
                };
                skills.retain(|existing| existing.name != meta.name);
                skills.push(meta);
            }
        }
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(skills)
    }

    /// System-prompt listing: names + descriptions only (bodies load on
    /// demand via the skill tool).
    pub fn listing_prompt(&self) -> anyhow::Result<String> {
        let skills = self.list()?;
        if skills.is_empty() {
            return Ok(String::new());
        }
        let lines: Vec<String> = skills
            .iter()
            .map(|s| {
                if s.triggers.is_empty() {
                    format!("- {}: {}", s.name, s.description)
                } else {
                    format!(
                        "- {}: {} (use when: {})",
                        s.name,
                        s.description,
                        s.triggers.join("; ")
                    )
                }
            })
            .collect();
        Ok(format!(
            "# Skills\n\nAvailable via the `skill` tool (loads the full instructions). \
             Invoke a skill whenever its description or cues match the task:\n{}",
            lines.join("\n")
        ))
    }

    /// One skill, body and all: the built-in text, or the one file the
    /// scan found for that name — no other body is read.
    pub fn load(&self, name: &str) -> anyhow::Result<Option<Skill>> {
        let found = |metas: Vec<SkillMeta>| metas.into_iter().find(|s| s.name == name);
        // Unknown to the inventory is not yet unknown: a skill written
        // since the scan is found by the one directory listing a
        // refresh costs.
        let meta = match found(self.list()?) {
            Some(meta) => meta,
            None => match found(self.refresh()?) {
                Some(meta) => meta,
                None => return Ok(None),
            },
        };
        let skill = match &meta.source {
            SkillSource::Builtin(text) => {
                parse_skill_md(&meta.name, text).expect("builtin skill parses")
            }
            SkillSource::File(path) => parse_skill_md(&meta.name, &read_skill(path)?)
                .with_context(|| format!("parsing skill definition {}", path.display()))?,
        };
        Ok(Some(skill))
    }
}

/// A skill file within [`MAX_SKILL_BYTES`], or the reason it is not.
fn read_skill(path: &std::path::Path) -> anyhow::Result<String> {
    let size = std::fs::metadata(path)
        .with_context(|| format!("reading skill definition {}", path.display()))?
        .len();
    anyhow::ensure!(
        size <= MAX_SKILL_BYTES,
        "skill definition {} is {size} bytes; the limit is {} KiB",
        path.display(),
        MAX_SKILL_BYTES / 1024
    );
    std::fs::read_to_string(path)
        .with_context(|| format!("reading skill definition {}", path.display()))
}

/// The `skill` tool: loads a skill body on invocation.
pub struct SkillTool {
    store: std::sync::Arc<SkillStore>,
}

impl SkillTool {
    pub fn new(store: std::sync::Arc<SkillStore>) -> Self {
        Self { store }
    }
}

#[derive(Deserialize)]
struct SkillInput {
    name: String,
}

impl crate::tools::Tool for SkillTool {
    fn name(&self) -> &'static str {
        "skill"
    }
    fn description(&self) -> &'static str {
        "Load a skill's full instructions by name. Use when a listed skill \
         matches the current task."
    }
    fn concurrency(&self) -> crate::tools::ToolConcurrency {
        crate::tools::ToolConcurrency::Concurrent
    }

    fn workspace_access(&self) -> crate::tools::WorkspaceAccess {
        crate::tools::WorkspaceAccess::ReadOnly
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"name": {"type": "string", "description": "A skill's name from the list in the prompt"}},
            "required": ["name"]
        })
    }
    fn run(
        &self,
        input: serde_json::Value,
        _ctx: crate::tools::ToolContext,
    ) -> crate::tools::ToolFuture {
        let store = self.store.clone();
        Box::pin(async move {
            let input: SkillInput = match crate::tools::parse_input(input, "skill") {
                Ok(v) => v,
                Err(error) => return error,
            };
            // A file read, however small, is not the async runtime's
            // to wait on: it goes to a blocking thread like any other.
            let loaded = {
                let store = store.clone();
                let name = input.name.clone();
                tokio::task::spawn_blocking(move || store.load(&name)).await
            };
            let loaded = match loaded {
                Ok(loaded) => loaded,
                Err(error) => {
                    return crate::tools::ToolOutput::error(format!(
                        "skill: loading skills: {error}"
                    ));
                }
            };
            match loaded {
                Ok(Some(skill)) => crate::tools::ToolOutput::text(format!(
                    "# Skill: {} — {}\n\n{}",
                    skill.name, skill.description, skill.body
                )),
                Ok(None) => {
                    let available: Vec<String> = match store.list() {
                        Ok(skills) => skills.into_iter().map(|skill| skill.name).collect(),
                        Err(error) => {
                            return crate::tools::ToolOutput::error(format!(
                                "skill: loading skills: {error:#}"
                            ));
                        }
                    };
                    crate::tools::ToolOutput::error(format!(
                        "skill: no skill named {:?}; available: {}",
                        input.name,
                        available.join(", ")
                    ))
                }
                Err(error) => {
                    crate::tools::ToolOutput::error(format!("skill: loading skills: {error:#}"))
                }
            }
        })
    }
}
