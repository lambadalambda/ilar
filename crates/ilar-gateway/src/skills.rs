//! The assistant's own skills: written by it, read by the core's
//! `skill` tool, counted for the weekly review.
//!
//! Two rules keep the library small, both from Hermes: "lessons, not
//! logs" — a skill is a distilled rule with its reason, never an
//! incident narrative — and patch a skill that exists before creating
//! one. The tool's description says both; the ledger under
//! `.usage.json` is what later decides what went stale.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use ilar::agent::LoopEvent;
use ilar::tools::{
    Tool, ToolConcurrency, ToolContext, ToolFuture, ToolOutput, WorkspaceAccess, parse_input,
};
use serde::{Deserialize, Serialize};

/// One skill's counters.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Usage {
    pub views: u64,
    pub patches: u64,
    pub created_at: Option<DateTime<Utc>>,
    pub last_viewed_at: Option<DateTime<Utc>>,
    pub last_patched_at: Option<DateTime<Utc>>,
}

/// `<home>/skills/`: `<name>/SKILL.md` each, the layout the core reads.
pub struct SkillLibrary {
    dir: PathBuf,
    write: Mutex<()>,
}

impl SkillLibrary {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            write: Mutex::new(()),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name).join("SKILL.md")
    }

    fn ledger_path(&self) -> PathBuf {
        self.dir.join(".usage.json")
    }

    /// A name is a directory and a slash command: lowercase letters,
    /// digits and dashes.
    fn check_name(name: &str) -> Result<()> {
        let ok = !name.is_empty()
            && name.len() <= 64
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && !name.starts_with('-');
        if !ok {
            bail!("skill name {name:?}: lowercase letters, digits and dashes only");
        }
        Ok(())
    }

    /// The text of a skill file: frontmatter the core parses, then the
    /// body.
    fn render(description: &str, triggers: &[String], body: &str) -> String {
        let mut text = format!("---\ndescription = {}\n", toml_string(description));
        if !triggers.is_empty() {
            let list = triggers
                .iter()
                .map(|t| toml_string(t))
                .collect::<Vec<_>>()
                .join(", ");
            text.push_str(&format!("triggers = [{list}]\n"));
        }
        text.push_str("---\n\n");
        text.push_str(body.trim());
        text.push('\n');
        text
    }

    pub fn create(
        &self,
        name: &str,
        description: &str,
        triggers: &[String],
        body: &str,
    ) -> Result<PathBuf> {
        Self::check_name(name)?;
        if description.trim().is_empty() || body.trim().is_empty() {
            bail!("a skill needs a description and a body");
        }
        let _write = self.write.lock().unwrap();
        let path = self.path(name);
        if path.exists() {
            bail!("skill {name} exists; patch it instead of creating it again");
        }
        crate::routes::write_atomically(
            &path,
            Self::render(description, triggers, body).as_bytes(),
        )?;
        self.touch(name, |usage| usage.created_at = Some(Utc::now()))?;
        Ok(path)
    }

    /// Replace one passage. `old` must occur exactly once, so a patch
    /// changes only what it names.
    pub fn patch(&self, name: &str, old: &str, new: &str) -> Result<()> {
        Self::check_name(name)?;
        if old.is_empty() {
            bail!("patch needs the text to replace");
        }
        let _write = self.write.lock().unwrap();
        let path = self.path(name);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("no skill {name} in {}", self.dir.display()))?;
        match text.matches(old).count() {
            1 => {}
            0 => bail!("skill {name} does not contain the text to replace"),
            n => bail!("skill {name} contains that text {n} times; be more specific"),
        }
        crate::routes::write_atomically(&path, text.replacen(old, new, 1).as_bytes())?;
        self.touch(name, |usage| {
            usage.patches += 1;
            usage.last_patched_at = Some(Utc::now());
        })
    }

    /// Rewrite a skill whole: the same file the core reads, new content.
    pub fn rewrite(
        &self,
        name: &str,
        description: &str,
        triggers: &[String],
        body: &str,
    ) -> Result<()> {
        Self::check_name(name)?;
        let _write = self.write.lock().unwrap();
        let path = self.path(name);
        if !path.exists() {
            bail!("no skill {name}; create it");
        }
        crate::routes::write_atomically(
            &path,
            Self::render(description, triggers, body).as_bytes(),
        )?;
        self.touch(name, |usage| {
            usage.patches += 1;
            usage.last_patched_at = Some(Utc::now());
        })
    }

    /// Move a skill aside, to `.archive/<name>`: out of the listing,
    /// still on disk.
    pub fn archive(&self, name: &str) -> Result<()> {
        Self::check_name(name)?;
        let _write = self.write.lock().unwrap();
        let from = self.dir.join(name);
        if !from.join("SKILL.md").exists() {
            bail!("no skill {name}");
        }
        let archive = self.dir.join(".archive");
        std::fs::create_dir_all(&archive)?;
        let to = archive.join(name);
        if to.exists() {
            std::fs::remove_dir_all(&to)?;
        }
        std::fs::rename(&from, &to)?;
        let mut ledger = self.ledger()?;
        ledger.remove(name);
        self.save_ledger(&ledger)
    }

    pub fn delete(&self, name: &str) -> Result<()> {
        Self::check_name(name)?;
        let _write = self.write.lock().unwrap();
        let dir = self.dir.join(name);
        if !dir.join("SKILL.md").exists() {
            bail!("no skill {name}");
        }
        std::fs::remove_dir_all(&dir)?;
        let mut ledger = self.ledger()?;
        ledger.remove(name);
        self.save_ledger(&ledger)
    }

    pub fn names(&self) -> Result<Vec<String>> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).context("reading skills"),
        };
        let mut names: Vec<String> = entries
            .filter_map(Result::ok)
            .filter(|entry| entry.path().join("SKILL.md").is_file())
            .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
            .collect();
        names.sort();
        Ok(names)
    }

    /// A view, as the core's `skill` tool loading it.
    pub fn note_view(&self, name: &str) -> Result<()> {
        let _write = self.write.lock().unwrap();
        self.touch(name, |usage| {
            usage.views += 1;
            usage.last_viewed_at = Some(Utc::now());
        })
    }

    pub fn ledger(&self) -> Result<BTreeMap<String, Usage>> {
        match std::fs::read_to_string(self.ledger_path()) {
            Ok(text) => serde_json::from_str(&text).context("parsing the skill ledger"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(error) => Err(error).context("reading the skill ledger"),
        }
    }

    fn save_ledger(&self, ledger: &BTreeMap<String, Usage>) -> Result<()> {
        crate::routes::write_atomically(&self.ledger_path(), &serde_json::to_vec_pretty(ledger)?)
    }

    fn touch(&self, name: &str, change: impl FnOnce(&mut Usage)) -> Result<()> {
        let mut ledger = self.ledger()?;
        change(ledger.entry(name.to_string()).or_default());
        self.save_ledger(&ledger)
    }
}

fn toml_string(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into())
}

/// Watches a turn's events for the core's `skill` tool loading a skill,
/// so the ledger counts views.
#[derive(Default)]
pub struct SkillWatch {
    skill_calls: std::collections::HashSet<String>,
}

impl SkillWatch {
    /// The name of a skill the model just loaded, if this event is that.
    pub fn observe(&mut self, event: &LoopEvent) -> Option<String> {
        match event {
            LoopEvent::ToolStarted { id, name } if name == "skill" => {
                self.skill_calls.insert(id.clone());
                None
            }
            LoopEvent::ToolInputComplete { id, arguments } if self.skill_calls.remove(id) => {
                serde_json::from_str::<serde_json::Value>(arguments)
                    .ok()?
                    .get("name")?
                    .as_str()
                    .map(str::to_string)
            }
            _ => None,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Action {
    Create,
    Patch,
    Rewrite,
    Delete,
}

#[derive(Deserialize)]
struct Input {
    action: Action,
    name: String,
    description: Option<String>,
    #[serde(default)]
    triggers: Vec<String>,
    body: Option<String>,
    old: Option<String>,
    new: Option<String>,
}

/// `skill_manage`: the model's hand on its own library.
pub struct SkillManageTool {
    library: std::sync::Arc<SkillLibrary>,
}

impl SkillManageTool {
    pub fn new(library: std::sync::Arc<SkillLibrary>) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self { library })
    }
}

impl Tool for SkillManageTool {
    fn name(&self) -> &'static str {
        "skill_manage"
    }

    fn description(&self) -> &'static str {
        "Keep your own skills: procedures worth repeating, found by the skill tool next time. \
         create (name, description, triggers, body), patch (name, old, new — old must occur \
         once), rewrite (name, description, triggers, body), delete (name). Two rules. Lessons, \
         not logs: a skill is a distilled rule with the reason attached — when to use it, the \
         procedure, the pitfalls, how to verify — never a narrative of what happened. And patch \
         before you create: a skill that exists and is close gets the lesson added; a new one \
         is the last resort."
    }

    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Barrier
    }

    fn workspace_access(&self) -> WorkspaceAccess {
        WorkspaceAccess::None
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["create", "patch", "rewrite", "delete"]},
                "name": {"type": "string", "description": "lowercase, digits, dashes; also the /command"},
                "description": {"type": "string", "description": "One line: what it is for"},
                "triggers": {"type": "array", "items": {"type": "string"}, "description": "Cue phrases that should invoke it"},
                "body": {"type": "string", "description": "Markdown: when to use, procedure, pitfalls, verification"},
                "old": {"type": "string", "description": "patch: the passage to replace, occurring once"},
                "new": {"type": "string", "description": "patch: its replacement"}
            },
            "required": ["action", "name"]
        })
    }

    fn run(&self, input: serde_json::Value, _ctx: ToolContext) -> ToolFuture {
        let library = self.library.clone();
        Box::pin(async move {
            let input: Input = match parse_input(input, "skill_manage") {
                Ok(input) => input,
                Err(error) => return error,
            };
            let outcome = match input.action {
                Action::Create => library
                    .create(
                        &input.name,
                        input.description.as_deref().unwrap_or(""),
                        &input.triggers,
                        input.body.as_deref().unwrap_or(""),
                    )
                    .map(|path| format!("created {} at {}", input.name, path.display())),
                Action::Patch => library
                    .patch(
                        &input.name,
                        input.old.as_deref().unwrap_or(""),
                        input.new.as_deref().unwrap_or(""),
                    )
                    .map(|()| format!("patched {}", input.name)),
                Action::Rewrite => library
                    .rewrite(
                        &input.name,
                        input.description.as_deref().unwrap_or(""),
                        &input.triggers,
                        input.body.as_deref().unwrap_or(""),
                    )
                    .map(|()| format!("rewrote {}", input.name)),
                Action::Delete => library
                    .delete(&input.name)
                    .map(|()| format!("deleted {}", input.name)),
            };
            match outcome {
                Ok(text) => ToolOutput::text(format!(
                    "{text}. It is listed in the prompt from the next session on; the skill tool loads it now."
                )),
                Err(error) => ToolOutput::error(format!("skill_manage: {error:#}")),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skills_are_created_in_the_layout_the_core_reads_and_patched_once() {
        let dir = tempfile::tempdir().unwrap();
        let library = SkillLibrary::new(dir.path().join("skills"));
        let path = library
            .create(
                "deploy-check",
                "Check a deploy",
                &["deploy".into(), "is it up".into()],
                "# Deploy check\n\n1. curl the health URL.\n2. tail the log.",
            )
            .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("---\ndescription = \"Check a deploy\"\ntriggers = [\"deploy\", \"is it up\"]\n---\n\n# Deploy check"), "{text}");
        assert!(library.create("deploy-check", "again", &[], "x").is_err());
        assert!(library.create("Bad Name", "x", &[], "x").is_err());
        library
            .patch("deploy-check", "tail the log", "tail the journal")
            .unwrap();
        assert!(library.patch("deploy-check", "absent", "x").is_err());
        library.patch("deploy-check", "curl", "fetch").unwrap();
        assert!(
            library.patch("deploy-check", "the", "a").is_err(),
            "occurs twice"
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("tail the journal") && text.contains("fetch"),
            "{text}"
        );
        library.note_view("deploy-check").unwrap();
        let ledger = library.ledger().unwrap();
        assert_eq!(ledger["deploy-check"].patches, 2);
        assert_eq!(ledger["deploy-check"].views, 1);
        assert!(ledger["deploy-check"].created_at.is_some());
        assert_eq!(library.names().unwrap(), ["deploy-check"]);
        library.delete("deploy-check").unwrap();
        assert!(library.names().unwrap().is_empty());
        assert!(library.ledger().unwrap().is_empty());
    }

    #[test]
    fn the_watch_sees_a_skill_being_loaded() {
        let mut watch = SkillWatch::default();
        assert_eq!(
            watch.observe(&LoopEvent::ToolStarted {
                id: "1".into(),
                name: "skill".into()
            }),
            None
        );
        assert_eq!(
            watch.observe(&LoopEvent::ToolInputComplete {
                id: "1".into(),
                arguments: "{\"name\": \"deploy-check\"}".into(),
            }),
            Some("deploy-check".into())
        );
        assert_eq!(
            watch.observe(&LoopEvent::ToolStarted {
                id: "2".into(),
                name: "bash".into()
            }),
            None
        );
        assert_eq!(
            watch.observe(&LoopEvent::ToolInputComplete {
                id: "2".into(),
                arguments: "{\"name\": \"not a skill\"}".into(),
            }),
            None
        );
    }
}
