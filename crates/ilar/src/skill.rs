//! Skills: markdown + frontmatter, discovered in the user config dir and
//! the project `.ilar/skills/`, loaded on demand via the `skill` tool.
//! Ships the worktree-isolation built-in. ~200 lines, kept dumb.

use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
}

/// The built-in worktree-isolation skill: run a subagent in a git worktree.
const WORKTREE_ISOLATION: &str = r#"---
description = "Run a subagent in an isolated git worktree so it can't touch the working tree"
---
# Worktree isolation

To run a task without touching the current working tree:

1. Create a worktree: `git worktree add ../ilar-task-<name> -b task/<name>`
2. Invoke the `task` tool with the subagent prompt, and prepend to the
   prompt: "You are working in the git worktree at ../ilar-task-<name>.
   Run all commands and edits there; never touch the main checkout."
3. When the subagent finishes, review the diff in the worktree
   (`git -C ../ilar-task-<name> diff`), merge or cherry-pick if good.
4. Clean up: `git worktree remove ../ilar-task-<name>` and delete the
   branch if abandoned.

Use for risky refactors or experiments that should not race concurrent
edits in the main checkout.
"#;

#[derive(Deserialize)]
struct Frontmatter {
    description: Option<String>,
}

fn parse_skill_md(name: &str, text: &str) -> Option<Skill> {
    let text = text.trim_start_matches('\u{feff}');
    let rest = text.strip_prefix("---\n")?;
    let (frontmatter, body) = rest.split_once("\n---")?;
    let fm: Frontmatter = toml::from_str(frontmatter).ok()?;
    Some(Skill {
        name: name.into(),
        description: fm.description.unwrap_or_else(|| name.into()),
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
    pub fn list(&self) -> Vec<Skill> {
        let mut skills = vec![
            parse_skill_md("worktree-isolation", WORKTREE_ISOLATION).expect("builtin skill parses"),
        ];
        for (dir, sub) in [
            (&self.user_dir, "skills"),
            (&self.project_dir, ".ilar/skills"),
        ] {
            let dir = dir.join(sub);
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|e| e != "md") {
                    continue;
                }
                let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                if let Ok(text) = std::fs::read_to_string(&path)
                    && let Some(skill) = parse_skill_md(name, &text)
                {
                    skills.retain(|s| s.name != skill.name);
                    skills.push(skill);
                }
            }
        }
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        skills
    }

    /// System-prompt listing: names + descriptions only (bodies load on
    /// demand via the skill tool).
    pub fn listing_prompt(&self) -> String {
        let skills = self.list();
        if skills.is_empty() {
            return String::new();
        }
        let lines: Vec<String> = skills
            .iter()
            .map(|s| format!("- {}: {}", s.name, s.description))
            .collect();
        format!(
            "# Skills\n\nAvailable via the `skill` tool (loads the full instructions):\n{}",
            lines.join("\n")
        )
    }

    pub fn load(&self, name: &str) -> Option<Skill> {
        self.list().into_iter().find(|s| s.name == name)
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
    fn kind(&self) -> crate::tools::ToolKind {
        crate::tools::ToolKind::ReadOnly
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
                Some(skill) => crate::tools::ToolOutput::text(format!(
                    "# Skill: {} — {}\n\n{}",
                    skill.name, skill.description, skill.body
                )),
                None => {
                    let available: Vec<String> =
                        store.list().iter().map(|s| s.name.clone()).collect();
                    crate::tools::ToolOutput::error(format!(
                        "unknown skill {:?}; available: {}",
                        input.name,
                        available.join(", ")
                    ))
                }
            }
        })
    }
}
