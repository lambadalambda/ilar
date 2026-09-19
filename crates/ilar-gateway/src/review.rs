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
use ilar::memory::{CoreFile, MemoryStore, NoteKind, SUMMARY_RULE};
use serde::{Deserialize, Serialize};

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
    /// The model wrote its own memory during this stretch. A review
    /// would be a second opinion on a decision already taken, and the
    /// two write the same fact twice.
    pub wrote_memory: bool,
}

impl Episode {
    pub fn observe(&mut self, event: &LoopEvent) {
        match event {
            LoopEvent::TurnStarted => self.turns += 1,
            LoopEvent::ToolFinished { name, is_error, .. } => {
                self.tool_calls += 1;
                if *is_error {
                    self.errors += 1;
                } else if name == "memory" {
                    self.wrote_memory = true;
                }
            }
            _ => {}
        }
    }

    /// Hermes's threshold: enough tool calls, or something went wrong
    /// and was recovered from — and nobody already did the job.
    pub fn worth_reviewing(&self, min_tool_calls: usize) -> bool {
        !self.wrote_memory
            && self.turns > 0
            && (self.tool_calls >= min_tool_calls || self.errors > 0)
    }
}

/// The instruction appended after the conversation. Carries the same
/// summary rule the `memory` tool states, so a note written here is
/// found the same way as one the model wrote itself.
pub static PROMPT: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    format!(
        "Review this conversation since your last review, as yourself. Is there \
anything durable in it — a preference or correction from the person, a fact about your world \
that will still be true next week, a decision, a workflow that worked or a dead end to avoid? \
Most conversations have nothing durable; then answer with exactly the word nothing. Otherwise \
answer with one JSON object and nothing else: {{\"memory\": [{{\"file\": \"user\" or \"memory\", \
\"action\": \"add\" or \"replace\" or \"remove\", \"text\": \"…\", \"old\": \"…\", \"new\": \
\"…\"}}], \"notes\": [{{\"kind\": \"decision\"|\"solution\"|\"preference\"|\"event\"|\"task\"|\
\"risk\", \"title\": \"…\", \"summary\": \"one line\", \"body\": \"the fact in full\"}}]}}. \
A note the conversation changed or disproved is not a second note: search the archive first, \
and answer with {{\"action\": \"amend\", \"id\": \"…\"}} plus the fields to change, or \
{{\"action\": \"forget\", \"id\": \"…\"}}, in the same notes list. \
{} \
Memory entries are one short line each and the files are small: prefer replace over add \
when an entry is already about the same thing. A workflow worth repeating is a skill, not a \
memory entry: add \"skills\": [{{\"action\": \"create\" or \"patch\", \"name\": \"lowercase-with-\
dashes\", \"description\": \"…\", \"triggers\": [\"…\"], \"body\": \"…\", \"old\": \"…\", \
\"new\": \"…\"}}] — a distilled rule with its reason, never the story of what happened, and \
patch a skill that exists before creating one. Never store secrets, paths that change, or \
what is easily looked up.",
        SUMMARY_RULE
    )
});

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
    /// `note` (the default), `amend` or `forget`; the last two name a
    /// note by `id` instead of describing a new one.
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub kind: Option<NoteKind>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
}

impl NoteDraft {
    fn action(&self) -> &str {
        self.action.as_deref().unwrap_or("note")
    }

    /// What the chat is told this draft is about: the title, or the id
    /// for a draft that only names one.
    fn subject(&self) -> String {
        self.title
            .clone()
            .or_else(|| self.id.clone())
            .unwrap_or_else(|| "?".into())
    }
}

/// What the review's answer amounted to.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    /// "nothing", or an empty plan: the episode is done with.
    Nothing,
    /// A plan with at least one change.
    Plan(Plan),
    /// Not an answer to the question; the episode is kept for the
    /// next review.
    Unparsed,
}

impl Answer {
    /// Tolerant of prose around the object and of a code fence.
    pub fn parse(answer: &str) -> Self {
        let trimmed = answer.trim().trim_matches('`').trim();
        let word = trimmed.trim_end_matches(['.', '!']);
        if word.eq_ignore_ascii_case("nothing") || word.is_empty() {
            return Self::Nothing;
        }
        let (Some(start), Some(end)) = (trimmed.find('{'), trimmed.rfind('}')) else {
            return Self::Unparsed;
        };
        if end <= start {
            return Self::Unparsed;
        }
        match serde_json::from_str::<Plan>(&trimmed[start..=end]) {
            Ok(plan) if plan.is_empty() => Self::Nothing,
            Ok(plan) => Self::Plan(plan),
            Err(_) => Self::Unparsed,
        }
    }
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.memory.is_empty() && self.notes.is_empty() && self.skills.is_empty()
    }

    /// A plan out of the model's answer, if it holds one.
    pub fn parse(answer: &str) -> Option<Self> {
        match Answer::parse(answer) {
            Answer::Plan(plan) => Some(plan),
            _ => None,
        }
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
            lines.push(match note.action() {
                "note" => format!(
                    "note ({}): {}",
                    note.kind.map(kind_name).unwrap_or("?"),
                    note.subject()
                ),
                other => format!("note {other}: {}", note.subject()),
            });
        }
        for skill in &self.skills {
            lines.push(format!("skill {}: {}", skill.action, skill.name));
        }
        lines
    }

    /// Write the plan through the store. Every edit is attempted; what
    /// landed and what did not are kept apart, so a chat is never told
    /// it remembered something that failed to be written.
    pub fn apply(&self, store: &MemoryStore, skills: &crate::skills::SkillLibrary) -> Applied {
        let mut outcome = Applied::default();
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
            match result {
                Ok(()) => outcome
                    .kept
                    .push(format!("skill {}: {}", skill.action, skill.name)),
                Err(error) => outcome
                    .failed
                    .push(format!("skill {}: {error:#}", skill.name)),
            }
        }
        for edit in &self.memory {
            let file = file_name(edit.file);
            let text = edit.text.as_deref().unwrap_or("");
            let result = match edit.action.as_str() {
                "add" => store.add(edit.file, text).map(|added| {
                    if added {
                        format!("{file}: {text}")
                    } else {
                        format!("{file}: already had {text}")
                    }
                }),
                "replace" => store
                    .replace(
                        edit.file,
                        edit.old.as_deref().unwrap_or(""),
                        edit.new.as_deref().unwrap_or(""),
                    )
                    .map(|()| format!("{file}: {}", edit.new.clone().unwrap_or_default())),
                "remove" => store
                    .remove(edit.file, text)
                    .map(|dropped| format!("{file}: removed {}", dropped.join("; "))),
                other => Err(anyhow::anyhow!("unknown action {other:?}")),
            };
            match result {
                Ok(line) => outcome.kept.push(line),
                Err(error) => outcome.failed.push(format!("{file}: {error:#}")),
            }
        }
        for note in &self.notes {
            let result = match (note.action(), note.id.as_deref()) {
                ("note", _) => match (note.kind, note.title.as_deref(), note.summary.as_deref()) {
                    (Some(kind), Some(title), Some(summary)) => store
                        .note(
                            kind,
                            title,
                            summary,
                            note.body.as_deref().unwrap_or(summary),
                            Utc::now(),
                        )
                        .map(|written| format!("note {}: {title}", written.id)),
                    _ => Err(anyhow::anyhow!("a note needs kind, title and summary")),
                },
                ("amend", Some(id)) => store
                    .amend(
                        id,
                        ilar::memory::Amendment {
                            kind: note.kind,
                            title: note.title.as_deref(),
                            summary: note.summary.as_deref(),
                            body: note.body.as_deref(),
                        },
                    )
                    .map(|amended| format!("note {id} amended: {}", amended.title)),
                ("forget", Some(id)) => store.forget(id).map(|()| format!("note {id} forgotten")),
                ("amend" | "forget", None) => {
                    Err(anyhow::anyhow!("{} needs the note's id", note.action()))
                }
                (other, _) => Err(anyhow::anyhow!("unknown note action {other:?}")),
            };
            match result {
                Ok(line) => outcome.kept.push(line),
                Err(error) => outcome
                    .failed
                    .push(format!("note {}: {error:#}", note.subject())),
            }
        }
        outcome
    }
}

/// What applying a plan came to: what the store now holds, and what it
/// refused.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Applied {
    pub kept: Vec<String>,
    pub failed: Vec<String>,
}

impl Applied {
    /// The line the chat gets. A failure is never dressed as a memory.
    pub fn report(&self) -> String {
        let kept = self.kept.join("; ");
        let failed = self.failed.join("; ");
        match (self.kept.is_empty(), self.failed.is_empty()) {
            (true, true) => "Nothing to remember.".to_string(),
            (true, false) => format!("⚠ nothing kept: {failed}"),
            (false, true) => format!("💾 remembered: {kept}"),
            (false, false) => format!("💾 remembered: {kept} — not kept: {failed}"),
        }
    }
}

/// Several plans applied in one go — `/approve all` — read as one.
impl FromIterator<Applied> for Applied {
    fn from_iter<I: IntoIterator<Item = Applied>>(parts: I) -> Self {
        let mut all = Self::default();
        for part in parts {
            all.kept.extend(part.kept);
            all.failed.extend(part.failed);
        }
        all
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

    /// The rule the `memory` tool states, so a note written here is
    /// found the same way as one the model wrote itself.
    #[test]
    fn the_review_prompt_carries_the_summary_rule() {
        assert!(PROMPT.contains(SUMMARY_RULE));
        assert!(PROMPT.contains("\"summary\": \"one line\""));
    }

    /// A conversation that changed a fact does not file a second note
    /// about it: the reviewer amends the one that is there, and
    /// retires one the work disproved.
    #[test]
    fn a_review_amends_and_forgets_the_notes_it_names() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(dir.path().to_path_buf());
        let skills = crate::skills::SkillLibrary::new(dir.path().join("skills"));
        let now = Utc::now();
        let moved = store
            .note(NoteKind::Decision, "Deploy box", "on tenco", "old", now)
            .unwrap();
        let wrong = store
            .note(
                NoteKind::Risk,
                "Disk",
                "the disk is nearly full",
                "80%",
                now,
            )
            .unwrap();

        let plan = Plan::parse(&format!(
            "{{\"notes\": [\
             {{\"action\": \"amend\", \"id\": \"{}\", \"summary\": \"on secunda now\"}}, \
             {{\"action\": \"forget\", \"id\": \"{}\"}}, \
             {{\"action\": \"forget\"}}]}}",
            moved.id, wrong.id
        ))
        .expect("a plan");
        let described = plan.describe();
        assert_eq!(described[0], format!("note amend: {}", moved.id));
        assert_eq!(described[1], format!("note forget: {}", wrong.id));

        let applied = plan.apply(&store, &skills);
        assert_eq!(
            applied.kept,
            [
                format!("note {} amended: Deploy box", moved.id),
                format!("note {} forgotten", wrong.id),
            ]
        );
        assert_eq!(applied.failed.len(), 1, "{:?}", applied.failed);
        assert!(applied.failed[0].contains("needs the note's id"));

        let left = store.notes().unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].summary, "on secunda now");
        assert_eq!(left[0].id, moved.id, "amended, not replaced");
    }

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

        // The model that kept its own memory has already answered the
        // question the review asks.
        let mut kept = Episode::default();
        kept.observe(&LoopEvent::TurnStarted);
        kept.observe(&finished(false));
        kept.observe(&LoopEvent::ToolFinished {
            id: "2".into(),
            name: "memory".into(),
            is_error: false,
            result: "added".into(),
            child_session_id: None,
        });
        assert!(kept.tool_calls >= 2 && !kept.worth_reviewing(2));
        let mut failed = Episode::default();
        failed.observe(&LoopEvent::TurnStarted);
        failed.observe(&finished(true));
        assert!(failed.worth_reviewing(5));
    }

    #[test]
    fn answers_parse_into_plans_and_nothing_is_nothing() {
        assert_eq!(Answer::parse("nothing"), Answer::Nothing);
        assert_eq!(Answer::parse("  Nothing.  "), Answer::Nothing);
        assert_eq!(Answer::parse("{}"), Answer::Nothing);
        assert_eq!(Answer::parse("no json here"), Answer::Unparsed);
        assert_eq!(Answer::parse("{not json}"), Answer::Unparsed);
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
        assert_eq!(outcome.kept[0], "user: Likes tea");
        assert!(outcome.kept[1].starts_with("note "), "{outcome:?}");
        assert_eq!(outcome.kept.len(), 2, "{outcome:?}");
        assert_eq!(outcome.failed.len(), 1, "{outcome:?}");
        assert!(outcome.failed[0].starts_with("user: "), "{outcome:?}");
        assert_eq!(store.core(CoreFile::User).unwrap(), "Likes tea\n");
        assert_eq!(store.notes().unwrap().len(), 1);
        // The chat hears what was kept and what was not, apart.
        let report = outcome.report();
        assert!(
            report.starts_with("💾 remembered: user: Likes tea"),
            "{report}"
        );
        assert!(report.contains(" — not kept: user: "), "{report}");
    }

    #[test]
    fn a_failure_is_never_reported_as_a_memory() {
        let nothing = Applied::default();
        assert_eq!(nothing.report(), "Nothing to remember.");
        let failed = Applied {
            kept: vec![],
            failed: vec!["user: absent".into()],
        };
        assert_eq!(failed.report(), "⚠ nothing kept: user: absent");
        let both: Applied = [
            Applied {
                kept: vec!["user: tea".into()],
                failed: vec![],
            },
            failed,
        ]
        .into_iter()
        .collect();
        assert_eq!(
            both.report(),
            "💾 remembered: user: tea — not kept: user: absent"
        );
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
