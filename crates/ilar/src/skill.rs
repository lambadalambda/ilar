//! Skills: markdown + frontmatter, discovered in the user config dir and
//! the project `.ilar/skills/`, loaded on demand via the `skill` tool.
//! Ships the worktree-isolation built-in. ~200 lines, kept dumb.

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
   Omit it when a nested task simply inherited its immediate parent's worktree.
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Frontmatter {
    description: Option<String>,
    triggers: Option<Vec<String>>,
}

fn parse_skill_md(name: &str, text: &str) -> anyhow::Result<Skill> {
    let (frontmatter, body) = crate::config::split_frontmatter(text)?;
    let fm: Frontmatter = toml::from_str(&frontmatter).context("invalid skill frontmatter")?;
    Ok(Skill {
        name: name.into(),
        description: fm.description.unwrap_or_else(|| name.into()),
        triggers: fm.triggers.unwrap_or_default(),
        body: body.trim_start_matches('\n').trim().to_string(),
    })
}

pub struct SkillStore {
    user_dir: PathBuf,
    project_dir: PathBuf,
}

impl SkillStore {
    pub fn new(user_dir: PathBuf, project_dir: PathBuf) -> Self {
        Self {
            user_dir,
            project_dir,
        }
    }

    /// All available skills: built-ins, user dir, project .ilar/skills
    /// (later wins by name).
    pub fn list(&self) -> anyhow::Result<Vec<Skill>> {
        let mut skills = vec![
            parse_skill_md("worktree-isolation", WORKTREE_ISOLATION).expect("builtin skill parses"),
            parse_skill_md("mcp-via-cli", MCP_VIA_CLI).expect("builtin skill parses"),
        ];
        for (dir, sub) in [
            (&self.user_dir, "skills"),
            (&self.project_dir, ".ilar/skills"),
        ] {
            let dir = dir.join(sub);
            for path in crate::config::markdown_files(&dir)? {
                let name = path
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .with_context(|| format!("skill filename is not UTF-8: {}", path.display()))?;
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("reading skill definition {}", path.display()))?;
                let skill = parse_skill_md(name, &text)
                    .with_context(|| format!("parsing skill definition {}", path.display()))?;
                skills.retain(|existing| existing.name != skill.name);
                skills.push(skill);
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

    pub fn load(&self, name: &str) -> anyhow::Result<Option<Skill>> {
        Ok(self.list()?.into_iter().find(|s| s.name == name))
    }
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
            "properties": {"name": {"type": "string"}},
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
            let input: SkillInput = match serde_json::from_value(input) {
                Ok(v) => v,
                Err(e) => {
                    return crate::tools::ToolOutput::error(format!(
                        "invalid input for skill: {e}"
                    ));
                }
            };
            match store.load(&input.name) {
                Ok(Some(skill)) => crate::tools::ToolOutput::text(format!(
                    "# Skill: {} — {}\n\n{}",
                    skill.name, skill.description, skill.body
                )),
                Ok(None) => {
                    let available: Vec<String> = match store.list() {
                        Ok(skills) => skills.into_iter().map(|skill| skill.name).collect(),
                        Err(error) => {
                            return crate::tools::ToolOutput::error(format!(
                                "loading skills: {error:#}"
                            ));
                        }
                    };
                    crate::tools::ToolOutput::error(format!(
                        "unknown skill {:?}; available: {}",
                        input.name,
                        available.join(", ")
                    ))
                }
                Err(error) => crate::tools::ToolOutput::error(format!("loading skills: {error:#}")),
            }
        })
    }
}
