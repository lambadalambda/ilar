//! The review after a turn: what was worth keeping from this stretch of
//! conversation, asked once per idle episode while the provider's
//! cache is still warm.
//!
//! Hermes runs its review after every turn and tells it to be active.
//! This one runs once, just before the cache window closes, only when
//! the episode crossed a threshold, and is told that "nothing" is a
//! fine answer. It is an aside: the model sees the conversation and
//! answers the review instruction instead, and neither is recorded.
//! The answer is a plan — memory entries, notes — which the gateway
//! applies through the memory store, or stages for approval.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;
use ilar::agent::LoopEvent;
use serde::{Deserialize, Serialize};

use crate::memory::{CoreFile, MemoryStore, NoteKind};

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReviewConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// An episode with fewer tool calls than this, no error and no
    /// correction is not reviewed.
    #[serde(default = "default_min_tool_calls")]
    pub min_tool_calls: usize,
    /// Seconds of quiet after a turn before the review runs; when
    /// unset, just before the provider's cache window closes.
    pub after_idle_secs: Option<u64>,
    /// Stage what the review wants to write under `<home>/pending/`
    /// for `/pending` and `/approve`, instead of writing it.
    #[serde(default)]
    pub approval: bool,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_tool_calls: default_min_tool_calls(),
            after_idle_secs: None,
            approval: false,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_min_tool_calls() -> usize {
    5
}

/// What happened on a seat since its last review.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Episode {
    pub turns: usize,
    pub tool_calls: usize,
    pub errors: usize,
}

impl Episode {
    pub fn observe(&mut self, event: &LoopEvent) {
        match event {
            LoopEvent::TurnStarted => self.turns += 1,
            LoopEvent::ToolFinished { is_error, .. } => {
                self.tool_calls += 1;
                if *is_error {
                    self.errors += 1;
                }
            }
            _ => {}
        }
    }

    /// Hermes's threshold: enough tool calls, or something went wrong
    /// and was recovered from.
    pub fn worth_reviewing(&self, min_tool_calls: usize) -> bool {
        self.turns > 0 && (self.tool_calls >= min_tool_calls || self.errors > 0)
    }
}

/// The instruction appended after the conversation.
pub const PROMPT: &str = "Review this conversation since your last review, as yourself. Is there \
anything durable in it — a preference or correction from the person, a fact about your world \
that will still be true next week, a decision, a workflow that worked or a dead end to avoid? \
Most conversations have nothing durable; then answer with exactly the word nothing. Otherwise \
answer with one JSON object and nothing else: {\"memory\": [{\"file\": \"user\" or \"memory\", \
\"action\": \"add\" or \"replace\" or \"remove\", \"text\": \"…\", \"old\": \"…\", \"new\": \
\"…\"}], \"notes\": [{\"kind\": \"decision\"|\"solution\"|\"preference\"|\"event\"|\"task\"|\
\"risk\", \"title\": \"…\", \"summary\": \"one line\", \"body\": \"the fact in full\"}]}. \
Memory entries are one short line each and the files are small: prefer replace over add \
when an entry is already about the same thing. A workflow worth repeating is a skill, not a \
memory entry: add \"skills\": [{\"action\": \"create\" or \"patch\", \"name\": \"lowercase-with-\
dashes\", \"description\": \"…\", \"triggers\": [\"…\"], \"body\": \"…\", \"old\": \"…\", \
\"new\": \"…\"}] — a distilled rule with its reason, never the story of what happened, and \
patch a skill that exists before creating one. Never store secrets, paths that change, or \
what is easily looked up.";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Plan {
    #[serde(default)]
    pub memory: Vec<MemoryEdit>,
    #[serde(default)]
    pub notes: Vec<NoteDraft>,
    #[serde(default)]
    pub skills: Vec<SkillEdit>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillEdit {
    pub action: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub triggers: Vec<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub old: Option<String>,
    #[serde(default)]
    pub new: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryEdit {
    #[serde(default = "default_file")]
    pub file: CoreFile,
    pub action: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub old: Option<String>,
    #[serde(default)]
    pub new: Option<String>,
}

fn default_file() -> CoreFile {
    CoreFile::Memory
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NoteDraft {
    pub kind: NoteKind,
    pub title: String,
    pub summary: String,
    #[serde(default)]
    pub body: Option<String>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.memory.is_empty() && self.notes.is_empty() && self.skills.is_empty()
    }

    /// The model's answer as a plan: `None` for "nothing", an empty
    /// plan for an answer that was not one. Tolerant of prose around
    /// the object and of a code fence.
    pub fn parse(answer: &str) -> Option<Self> {
        let trimmed = answer.trim().trim_matches('`').trim();
        if trimmed.eq_ignore_ascii_case("nothing") || trimmed.is_empty() {
            return None;
        }
        let start = trimmed.find('{')?;
        let end = trimmed.rfind('}')?;
        if end <= start {
            return None;
        }
        serde_json::from_str(&trimmed[start..=end]).ok()
    }

    /// One line per change, for the chat.
    pub fn describe(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for edit in &self.memory {
            let what = match edit.action.as_str() {
                "add" => edit.text.clone().unwrap_or_default(),
                "replace" => edit.new.clone().unwrap_or_default(),
                "remove" => format!("(removed) {}", edit.text.clone().unwrap_or_default()),
                other => format!("({other}?)"),
            };
            lines.push(format!("{}: {what}", file_name(edit.file)));
        }
        for note in &self.notes {
            lines.push(format!("note ({}): {}", kind_name(note.kind), note.title));
        }
        for skill in &self.skills {
            lines.push(format!("skill {}: {}", skill.action, skill.name));
        }
        lines
    }

    /// Write the plan through the store. Every edit is attempted; the
    /// outcome lines say what landed and what did not.
    pub fn apply(&self, store: &MemoryStore, skills: &crate::skills::SkillLibrary) -> Vec<String> {
        let mut outcome = Vec::new();
        for skill in &self.skills {
            let result = match skill.action.as_str() {
                "create" => skills
                    .create(
                        &skill.name,
                        skill.description.as_deref().unwrap_or(""),
                        &skill.triggers,
                        skill.body.as_deref().unwrap_or(""),
                    )
                    .map(drop),
                "patch" => skills.patch(
                    &skill.name,
                    skill.old.as_deref().unwrap_or(""),
                    skill.new.as_deref().unwrap_or(""),
                ),
                other => Err(anyhow::anyhow!("unknown skill action {other:?}")),
            };
            outcome.push(match result {
                Ok(()) => format!("skill {}: {}", skill.action, skill.name),
                Err(error) => format!("skill {} not written — {error:#}", skill.name),
            });
        }
        for edit in &self.memory {
            let result = match edit.action.as_str() {
                "add" => store.add(edit.file, edit.text.as_deref().unwrap_or("")),
                "replace" => store.replace(
                    edit.file,
                    edit.old.as_deref().unwrap_or(""),
                    edit.new.as_deref().unwrap_or(""),
                ),
                "remove" => store.remove(edit.file, edit.text.as_deref().unwrap_or("")),
                other => Err(anyhow::anyhow!("unknown action {other:?}")),
            };
            let line = match result {
                Ok(()) => match edit.action.as_str() {
                    "add" => format!(
                        "{}: {}",
                        file_name(edit.file),
                        edit.text.clone().unwrap_or_default()
                    ),
                    "replace" => format!(
                        "{}: {}",
                        file_name(edit.file),
                        edit.new.clone().unwrap_or_default()
                    ),
                    _ => format!(
                        "{}: removed {}",
                        file_name(edit.file),
                        edit.text.clone().unwrap_or_default()
                    ),
                },
                Err(error) => format!("{}: not written — {error:#}", file_name(edit.file)),
            };
            outcome.push(line);
        }
        for note in &self.notes {
            let line = match store.note(
                note.kind,
                &note.title,
                &note.summary,
                note.body.as_deref().unwrap_or(&note.summary),
                Utc::now(),
            ) {
                Ok(written) => format!("note {}: {}", written.id, note.title),
                Err(error) => format!("note not written — {error:#}"),
            };
            outcome.push(line);
        }
        outcome
    }
}

fn file_name(file: CoreFile) -> &'static str {
    match file {
        CoreFile::Memory => "memory",
        CoreFile::User => "user",
    }
}

fn kind_name(kind: NoteKind) -> &'static str {
    match kind {
        NoteKind::Decision => "decision",
        NoteKind::Solution => "solution",
        NoteKind::Preference => "preference",
        NoteKind::Event => "event",
        NoteKind::Task => "task",
        NoteKind::Risk => "risk",
    }
}

/// A plan waiting for approval.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Pending {
    pub id: String,
    pub chat: String,
    pub when: chrono::DateTime<Utc>,
    pub plan: Plan,
}

/// Plans staged under `<home>/pending/`, one file each.
pub struct PendingStore {
    dir: PathBuf,
}

impl PendingStore {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn stage(&self, chat: &str, plan: Plan) -> Result<Pending> {
        let pending = Pending {
            id: ilar::session::new_id()[..8].to_string(),
            chat: chat.to_string(),
            when: Utc::now(),
            plan,
        };
        crate::routes::write_atomically(
            &self.dir.join(format!("{}.json", pending.id)),
            &serde_json::to_vec_pretty(&pending)?,
        )?;
        Ok(pending)
    }

    pub fn list(&self) -> Result<Vec<Pending>> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).context("reading pending"),
        };
        let mut pending: Vec<Pending> = entries
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
            .filter_map(|entry| std::fs::read(entry.path()).ok())
            .filter_map(|bytes| serde_json::from_slice(&bytes).ok())
            .collect();
        pending.sort_by_key(|p| p.when);
        Ok(pending)
    }

    /// Take one, or all with `"all"`, out of the store.
    pub fn take(&self, id: &str) -> Result<Vec<Pending>> {
        let taken: Vec<Pending> = self
            .list()?
            .into_iter()
            .filter(|p| id == "all" || p.id == id)
            .collect();
        for p in &taken {
            let _ = std::fs::remove_file(self.dir.join(format!("{}.json", p.id)));
        }
        Ok(taken)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_episode_is_worth_reviewing_past_the_threshold_or_after_an_error() {
        let mut episode = Episode::default();
        assert!(!episode.worth_reviewing(2));
        episode.observe(&LoopEvent::TurnStarted);
        assert!(!episode.worth_reviewing(2));
        let finished = |is_error| LoopEvent::ToolFinished {
            id: "1".into(),
            name: "bash".into(),
            is_error,
            result: String::new(),
            child_session_id: None,
        };
        episode.observe(&finished(false));
        assert!(!episode.worth_reviewing(2));
        episode.observe(&finished(false));
        assert!(episode.worth_reviewing(2));
        let mut failed = Episode::default();
        failed.observe(&LoopEvent::TurnStarted);
        failed.observe(&finished(true));
        assert!(failed.worth_reviewing(5));
    }

    #[test]
    fn answers_parse_into_plans_and_nothing_is_nothing() {
        assert_eq!(Plan::parse("nothing"), None);
        assert_eq!(Plan::parse("  Nothing.  "), None);
        assert_eq!(Plan::parse("no json here").map(|p| p.is_empty()), None);
        let plan = Plan::parse(
            "Here you go:\n```json\n{\"memory\": [{\"file\": \"user\", \"action\": \"add\", \
             \"text\": \"Likes earl grey\"}], \"notes\": [{\"kind\": \"decision\", \"title\": \
             \"Postgres\", \"summary\": \"chose postgres\"}]}\n```",
        )
        .unwrap();
        assert_eq!(plan.memory.len(), 1);
        assert_eq!(plan.notes.len(), 1);
        assert_eq!(
            plan.describe(),
            vec!["user: Likes earl grey", "note (decision): Postgres"]
        );
    }

    #[test]
    fn a_plan_applies_through_the_store_and_reports_each_change() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(dir.path().to_path_buf());
        let plan = Plan::parse(
            "{\"memory\": [{\"file\": \"user\", \"action\": \"add\", \"text\": \"Likes tea\"}, \
             {\"file\": \"user\", \"action\": \"remove\", \"text\": \"absent\"}], \
             \"notes\": [{\"kind\": \"event\", \"title\": \"Moved\", \"summary\": \"moved house\"}]}",
        )
        .unwrap();
        let skills = crate::skills::SkillLibrary::new(dir.path().join("skills"));
        let outcome = plan.apply(&store, &skills);
        assert_eq!(outcome[0], "user: Likes tea");
        assert!(outcome[1].contains("not written"), "{outcome:?}");
        assert!(outcome[2].starts_with("note "), "{outcome:?}");
        assert_eq!(store.core(CoreFile::User).unwrap(), "Likes tea\n");
        assert_eq!(store.notes().unwrap().len(), 1);
    }

    #[test]
    fn staged_plans_are_listed_and_taken() {
        let dir = tempfile::tempdir().unwrap();
        let pending = PendingStore::new(dir.path().join("pending"));
        assert!(pending.list().unwrap().is_empty());
        let plan = Plan::parse("{\"memory\": [{\"action\": \"add\", \"text\": \"x\"}]}").unwrap();
        let staged = pending.stage("fake:1", plan.clone()).unwrap();
        pending.stage("fake:1", plan).unwrap();
        assert_eq!(pending.list().unwrap().len(), 2);
        let taken = pending.take(&staged.id).unwrap();
        assert_eq!(taken.len(), 1);
        assert_eq!(pending.list().unwrap().len(), 1);
        assert_eq!(pending.take("all").unwrap().len(), 1);
        assert!(pending.list().unwrap().is_empty());
    }
}
