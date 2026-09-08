//! Memory that outlives a session.
//!
//! Two tiers, as every lightweight system converged on (DEVLOG,
//! 2026-09-08). A small core — `MEMORY.md`, the assistant's notes on
//! its world, and `USER.md`, on the person — with hard caps, injected
//! into the system prompt once per session and never in a group. And
//! an archive of one fact per file plus daily notes, never injected,
//! searched through tools with an index first and full notes on
//! request. Retrieval is ranking over a few hundred small files, so
//! it is done here rather than in a database: BM25 over words, with
//! recency decay so an old well-worded note does not beat yesterday's
//! update.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use ilar::tools::{
    Tool, ToolConcurrency, ToolContext, ToolFuture, ToolOutput, WorkspaceAccess, parse_input,
};
use serde::Deserialize;

use crate::routes::write_atomically;

/// Hermes's caps, which keep the core under a thousand tokens.
pub const MEMORY_CHARS: usize = 2200;
pub const USER_CHARS: usize = 1375;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoreFile {
    Memory,
    User,
}

impl CoreFile {
    fn file_name(self) -> &'static str {
        match self {
            Self::Memory => "MEMORY.md",
            Self::User => "USER.md",
        }
    }

    fn cap(self) -> usize {
        match self {
            Self::Memory => MEMORY_CHARS,
            Self::User => USER_CHARS,
        }
    }
}

/// A note's kind: what Awareness calls a knowledge card, typed so a
/// reader can ask for decisions or risks and not events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteKind {
    Decision,
    Solution,
    Preference,
    Event,
    Task,
    Risk,
}

impl NoteKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Decision => "decision",
            Self::Solution => "solution",
            Self::Preference => "preference",
            Self::Event => "event",
            Self::Task => "task",
            Self::Risk => "risk",
        }
    }
}

/// One archived fact, as read back from its file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub summary: String,
    pub when: DateTime<Utc>,
    pub body: String,
}

/// An index entry: what a search returns before the reader asks for
/// the whole note. About eighty tokens each.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub summary: String,
    pub when: DateTime<Utc>,
    pub score: f64,
}

pub struct MemoryStore {
    dir: PathBuf,
    /// Every change is a read-modify-write of a whole file, and chats
    /// run at once: one writer at a time.
    write: Mutex<()>,
}

impl MemoryStore {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            write: Mutex::new(()),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn core_path(&self, file: CoreFile) -> PathBuf {
        self.dir.join(file.file_name())
    }

    /// A core file's text; empty when it does not exist yet.
    pub fn core(&self, file: CoreFile) -> Result<String> {
        match std::fs::read_to_string(self.core_path(file)) {
            Ok(text) => Ok(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(error) => Err(error).with_context(|| format!("reading {}", file.file_name())),
        }
    }

    fn write_core(&self, file: CoreFile, text: &str) -> Result<()> {
        if text.chars().count() > file.cap() {
            bail!(
                "{} would be {} characters; the cap is {}. Consolidate or remove entries first.",
                file.file_name(),
                text.chars().count(),
                file.cap()
            );
        }
        write_atomically(&self.core_path(file), text.as_bytes())
    }

    /// Append one entry, a line of its own.
    pub fn add(&self, file: CoreFile, entry: &str) -> Result<()> {
        let entry = entry.trim();
        if entry.is_empty() {
            bail!("nothing to add");
        }
        let _write = self.write.lock().unwrap();
        let mut text = self.core(file)?;
        if entries(&text).any(|line| line == entry) {
            return Ok(());
        }
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(entry);
        text.push('\n');
        self.write_core(file, &text)
    }

    /// Replace the first entry that contains `old` with `new`.
    pub fn replace(&self, file: CoreFile, old: &str, new: &str) -> Result<()> {
        if old.trim().is_empty() || new.trim().is_empty() {
            bail!("replace needs both the text to find and the new entry");
        }
        let _write = self.write.lock().unwrap();
        let text = self.core(file)?;
        let mut found = false;
        let rewritten: Vec<&str> = entries(&text)
            .map(|line| {
                if !found && line.contains(old.trim()) {
                    found = true;
                    new.trim()
                } else {
                    line
                }
            })
            .filter(|line| !line.is_empty())
            .collect();
        if !found {
            bail!("no entry contains {old:?}");
        }
        self.write_core(file, &joined(&rewritten))
    }

    /// Remove every entry that contains `entry`.
    pub fn remove(&self, file: CoreFile, entry: &str) -> Result<()> {
        if entry.trim().is_empty() {
            bail!("remove needs the text of the entry");
        }
        let _write = self.write.lock().unwrap();
        let text = self.core(file)?;
        let kept: Vec<&str> = entries(&text)
            .filter(|line| !line.contains(entry.trim()))
            .collect();
        if kept.len() == entries(&text).count() {
            bail!("no entry contains {entry:?}");
        }
        self.write_core(file, &joined(&kept))
    }

    /// The block injected into a system prompt, or nothing when both
    /// files are empty.
    pub fn core_block(&self) -> Result<Option<String>> {
        let memory = self.core(CoreFile::Memory)?;
        let user = self.core(CoreFile::User)?;
        if memory.trim().is_empty() && user.trim().is_empty() {
            return Ok(None);
        }
        let mut block = String::from(
            "# Memory\n\nWhat you kept from earlier sessions. Frozen for this session; \
             the `memory` tool changes it for the next one.\n",
        );
        for (title, text, file) in [
            ("## About the world", &memory, CoreFile::Memory),
            ("## About the person", &user, CoreFile::User),
        ] {
            if text.trim().is_empty() {
                continue;
            }
            block.push_str(&format!(
                "\n{title} ({} of {} characters)\n\n{}",
                text.chars().count(),
                file.cap(),
                text.trim_end()
            ));
            block.push('\n');
        }
        Ok(Some(block))
    }

    /// One fact, its own file: `notes/<date>-<id>.md` with frontmatter.
    pub fn note(
        &self,
        kind: NoteKind,
        title: &str,
        summary: &str,
        body: &str,
        when: DateTime<Utc>,
    ) -> Result<Note> {
        let id = format!(
            "{}-{}",
            when.format("%Y%m%d"),
            &ilar::session::new_id()[..8]
        );
        let text = format!(
            "---\nid: {id}\nkind: {}\ntitle: {}\nsummary: {}\nwhen: {}\n---\n\n{}\n",
            kind.as_str(),
            title.trim().replace('\n', " "),
            summary.trim().replace('\n', " "),
            when.to_rfc3339(),
            body.trim()
        );
        write_atomically(
            &self.dir.join("notes").join(format!("{id}.md")),
            text.as_bytes(),
        )?;
        Ok(Note {
            id,
            kind: kind.as_str().into(),
            title: title.trim().into(),
            summary: summary.trim().into(),
            when,
            body: body.trim().into(),
        })
    }

    /// Append to today's daily note.
    pub fn daily(&self, when: DateTime<Utc>, heading: &str, text: &str) -> Result<PathBuf> {
        let path = self
            .dir
            .join("daily")
            .join(format!("{}.md", when.format("%Y-%m-%d")));
        let _write = self.write.lock().unwrap();
        let mut existing = std::fs::read_to_string(&path).unwrap_or_default();
        if existing.is_empty() {
            existing.push_str(&format!("# {}\n", when.format("%Y-%m-%d")));
        }
        existing.push_str(&format!(
            "\n## {} — {}\n\n{}\n",
            when.format("%H:%M"),
            heading.trim(),
            text.trim()
        ));
        write_atomically(&path, existing.as_bytes())?;
        Ok(path)
    }

    pub fn notes(&self) -> Result<Vec<Note>> {
        let dir = self.dir.join("notes");
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).context("reading notes"),
        };
        let mut notes = Vec::new();
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "md")
                && let Ok(text) = std::fs::read_to_string(&path)
                && let Some(note) = parse_note(&text)
            {
                notes.push(note);
            }
        }
        notes.sort_by_key(|note| std::cmp::Reverse(note.when));
        Ok(notes)
    }

    pub fn get(&self, ids: &[String]) -> Result<Vec<Note>> {
        let notes = self.notes()?;
        Ok(ids
            .iter()
            .filter_map(|id| notes.iter().find(|note| &note.id == id).cloned())
            .collect())
    }

    /// The index of what matches: BM25 over title, summary, kind and
    /// body, decayed by age, best first.
    pub fn search(&self, query: &str, limit: usize, now: DateTime<Utc>) -> Result<Vec<Hit>> {
        Ok(rank(&self.notes()?, query, now)
            .into_iter()
            .take(limit)
            .collect())
    }
}

fn entries(text: &str) -> impl Iterator<Item = &str> {
    text.lines().map(str::trim).filter(|line| !line.is_empty())
}

fn joined(lines: &[&str]) -> String {
    let mut text = lines.join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    text
}

fn parse_note(text: &str) -> Option<Note> {
    let rest = text.strip_prefix("---\n")?;
    let (front, body) = rest.split_once("\n---\n")?;
    let mut fields: HashMap<&str, &str> = HashMap::new();
    for line in front.lines() {
        if let Some((key, value)) = line.split_once(':') {
            fields.insert(key.trim(), value.trim());
        }
    }
    Some(Note {
        id: fields.get("id")?.to_string(),
        kind: fields.get("kind").unwrap_or(&"event").to_string(),
        title: fields.get("title").unwrap_or(&"").to_string(),
        summary: fields.get("summary").unwrap_or(&"").to_string(),
        when: DateTime::parse_from_rfc3339(fields.get("when")?)
            .ok()?
            .with_timezone(&Utc),
        body: body.trim().to_string(),
    })
}

fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.len() > 1)
        .map(str::to_string)
        .collect()
}

/// Half-life of a note's score by kind: what happened fades in a month,
/// what was decided or preferred holds for half a year.
fn half_life_days(kind: &str) -> f64 {
    match kind {
        "decision" | "preference" | "solution" => 180.0,
        _ => 30.0,
    }
}

/// BM25 with the usual constants, times a recency multiplier; notes
/// that match nothing are left out.
pub fn rank(notes: &[Note], query: &str, now: DateTime<Utc>) -> Vec<Hit> {
    let terms = words(query);
    if terms.is_empty() || notes.is_empty() {
        return Vec::new();
    }
    let documents: Vec<Vec<String>> = notes
        .iter()
        .map(|note| {
            words(&format!(
                "{} {} {} {}",
                note.kind, note.title, note.summary, note.body
            ))
        })
        .collect();
    let average = documents.iter().map(Vec::len).sum::<usize>() as f64 / documents.len() as f64;
    let (k1, b) = (1.2, 0.75);
    let idf: HashMap<&str, f64> = terms
        .iter()
        .map(|term| {
            let containing = documents
                .iter()
                .filter(|document| document.contains(term))
                .count() as f64;
            let idf = ((documents.len() as f64 - containing + 0.5) / (containing + 0.5) + 1.0).ln();
            (term.as_str(), idf)
        })
        .collect();
    let mut hits: Vec<Hit> = notes
        .iter()
        .zip(&documents)
        .filter_map(|(note, document)| {
            let mut score = 0.0;
            for term in &terms {
                let frequency = document.iter().filter(|word| *word == term).count() as f64;
                if frequency == 0.0 {
                    continue;
                }
                let length = document.len() as f64;
                score += idf[term.as_str()] * (frequency * (k1 + 1.0))
                    / (frequency + k1 * (1.0 - b + b * length / average.max(1.0)));
            }
            if score <= 0.0 {
                return None;
            }
            let age_days = (now - note.when).num_seconds().max(0) as f64 / 86_400.0;
            let decay = 0.5_f64.powf(age_days / half_life_days(&note.kind));
            Some(Hit {
                id: note.id.clone(),
                kind: note.kind.clone(),
                title: note.title.clone(),
                summary: note.summary.clone(),
                when: note.when,
                score: score * decay,
            })
        })
        .collect();
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    hits
}

// ---- tools ----

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum MemoryAction {
    Add,
    Replace,
    Remove,
    Note,
}

#[derive(Deserialize)]
struct MemoryInput {
    action: MemoryAction,
    #[serde(default = "default_file")]
    file: CoreFile,
    text: Option<String>,
    old: Option<String>,
    new: Option<String>,
    kind: Option<NoteKind>,
    title: Option<String>,
    summary: Option<String>,
    body: Option<String>,
}

fn default_file() -> CoreFile {
    CoreFile::Memory
}

/// `memory`: the core files and the archive, written by the model.
pub struct MemoryTool {
    store: Arc<MemoryStore>,
}

impl MemoryTool {
    pub fn new(store: Arc<MemoryStore>) -> Arc<Self> {
        Arc::new(Self { store })
    }
}

impl Tool for MemoryTool {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn description(&self) -> &'static str {
        "Remember across sessions. add / replace / remove change a core file (file: memory for \
         the world, user for the person) that is injected into every future session and has a \
         hard cap — an overflow is an error, so consolidate. note files one durable fact in the \
         archive (kind: decision, solution, preference, event, task, risk; title; a one-line \
         summary; body), found later with memory_search. Save preferences, corrections, \
         decisions and conventions; skip the trivial, the searchable, and today's paths."
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
                "action": {"type": "string", "enum": ["add", "replace", "remove", "note"]},
                "file": {"type": "string", "enum": ["memory", "user"], "description": "Which core file (default memory)"},
                "text": {"type": "string", "description": "add: the entry, one line; remove: text every entry to drop contains"},
                "old": {"type": "string", "description": "replace: text the first entry to replace contains"},
                "new": {"type": "string", "description": "replace: the new entry"},
                "kind": {"type": "string", "enum": ["decision", "solution", "preference", "event", "task", "risk"]},
                "title": {"type": "string"},
                "summary": {"type": "string", "description": "note: one line"},
                "body": {"type": "string", "description": "note: the fact in full (default: the summary)"}
            },
            "required": ["action"]
        })
    }

    fn run(&self, input: serde_json::Value, _ctx: ToolContext) -> ToolFuture {
        let store = self.store.clone();
        Box::pin(async move {
            let input: MemoryInput = match parse_input(input, "memory") {
                Ok(input) => input,
                Err(error) => return error,
            };
            let outcome = match input.action {
                MemoryAction::Add => input
                    .text
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("add needs text"))
                    .and_then(|text| store.add(input.file, text))
                    .map(|()| "added".to_string()),
                MemoryAction::Replace => match (input.old.as_deref(), input.new.as_deref()) {
                    (Some(old), Some(new)) => store
                        .replace(input.file, old, new)
                        .map(|()| "replaced".to_string()),
                    _ => Err(anyhow::anyhow!("replace needs old and new")),
                },
                MemoryAction::Remove => input
                    .text
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("remove needs text"))
                    .and_then(|text| store.remove(input.file, text))
                    .map(|()| "removed".to_string()),
                MemoryAction::Note => {
                    match (input.kind, input.title.as_deref(), input.summary.as_deref()) {
                        (Some(kind), Some(title), Some(summary)) => store
                            .note(
                                kind,
                                title,
                                summary,
                                input.body.as_deref().unwrap_or(summary),
                                Utc::now(),
                            )
                            .map(|note| format!("noted {} ({})", note.id, note.kind)),
                        _ => Err(anyhow::anyhow!("note needs kind, title and summary")),
                    }
                }
            };
            match outcome {
                Ok(text) => ToolOutput::text(text),
                Err(error) => ToolOutput::error(format!("memory: {error:#}")),
            }
        })
    }
}

#[derive(Deserialize)]
struct SearchInput {
    query: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    8
}

/// `memory_search`: the index, best first.
pub struct MemorySearchTool {
    store: Arc<MemoryStore>,
}

impl MemorySearchTool {
    pub fn new(store: Arc<MemoryStore>) -> Arc<Self> {
        Arc::new(Self { store })
    }
}

impl Tool for MemorySearchTool {
    fn name(&self) -> &'static str {
        "memory_search"
    }

    fn description(&self) -> &'static str {
        "Search the memory archive. Returns an index — id, kind, age, title, summary — best \
         first; read the ones that matter with memory_get. Recent notes rank higher."
    }

    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Concurrent
    }

    fn workspace_access(&self) -> WorkspaceAccess {
        WorkspaceAccess::None
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "limit": {"type": "integer", "description": "Default 8"}
            },
            "required": ["query"]
        })
    }

    fn run(&self, input: serde_json::Value, _ctx: ToolContext) -> ToolFuture {
        let store = self.store.clone();
        Box::pin(async move {
            let input: SearchInput = match parse_input(input, "memory_search") {
                Ok(input) => input,
                Err(error) => return error,
            };
            let now = Utc::now();
            match store.search(&input.query, input.limit.clamp(1, 50), now) {
                Ok(hits) if hits.is_empty() => ToolOutput::text("(no matching notes)"),
                Ok(hits) => ToolOutput::text(
                    hits.iter()
                        .map(|hit| {
                            format!(
                                "{} [{}] {} — {}: {}",
                                hit.id,
                                hit.kind,
                                age(now, hit.when),
                                hit.title,
                                hit.summary
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                Err(error) => ToolOutput::error(format!("memory_search: {error:#}")),
            }
        })
    }
}

fn age(now: DateTime<Utc>, when: DateTime<Utc>) -> String {
    let days = (now - when).num_days();
    match days {
        0 => "today".into(),
        1 => "yesterday".into(),
        d if d < 30 => format!("{d}d ago"),
        d => format!("{}mo ago", d / 30),
    }
}

#[derive(Deserialize)]
struct GetInput {
    ids: Vec<String>,
}

/// `memory_get`: the notes themselves, by id.
pub struct MemoryGetTool {
    store: Arc<MemoryStore>,
}

impl MemoryGetTool {
    pub fn new(store: Arc<MemoryStore>) -> Arc<Self> {
        Arc::new(Self { store })
    }
}

impl Tool for MemoryGetTool {
    fn name(&self) -> &'static str {
        "memory_get"
    }

    fn description(&self) -> &'static str {
        "Read archived notes in full, by the ids memory_search returned."
    }

    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Concurrent
    }

    fn workspace_access(&self) -> WorkspaceAccess {
        WorkspaceAccess::None
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"ids": {"type": "array", "items": {"type": "string"}}},
            "required": ["ids"]
        })
    }

    fn run(&self, input: serde_json::Value, _ctx: ToolContext) -> ToolFuture {
        let store = self.store.clone();
        Box::pin(async move {
            let input: GetInput = match parse_input(input, "memory_get") {
                Ok(input) => input,
                Err(error) => return error,
            };
            match store.get(&input.ids) {
                Ok(notes) if notes.is_empty() => ToolOutput::error("memory_get: no such notes"),
                Ok(notes) => ToolOutput::text(
                    notes
                        .iter()
                        .map(|note| {
                            format!(
                                "## {} [{}] {}\n{}\n\n{}",
                                note.id,
                                note.kind,
                                note.when.format("%Y-%m-%d"),
                                note.title,
                                note.body
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n\n"),
                ),
                Err(error) => ToolOutput::error(format!("memory_get: {error:#}")),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn core_entries_add_replace_remove_and_respect_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(dir.path().to_path_buf());
        assert_eq!(store.core_block().unwrap(), None);
        store.add(CoreFile::User, "Likes tea").unwrap();
        store.add(CoreFile::User, "Likes tea").unwrap();
        store
            .add(CoreFile::Memory, "The repo is at ~/repos/x")
            .unwrap();
        assert_eq!(store.core(CoreFile::User).unwrap(), "Likes tea\n");
        store
            .replace(CoreFile::User, "tea", "Likes coffee now")
            .unwrap();
        assert_eq!(store.core(CoreFile::User).unwrap(), "Likes coffee now\n");
        assert!(store.remove(CoreFile::User, "absent").is_err());
        assert!(store.remove(CoreFile::User, "  ").is_err());
        assert!(store.replace(CoreFile::User, "", "x").is_err());
        assert_eq!(store.core(CoreFile::User).unwrap(), "Likes coffee now\n");
        store.remove(CoreFile::User, "coffee").unwrap();
        assert_eq!(store.core(CoreFile::User).unwrap(), "");
        let block = store.core_block().unwrap().unwrap();
        assert!(block.contains("## About the world"), "{block}");
        assert!(!block.contains("## About the person"), "{block}");
        let too_long = "x".repeat(USER_CHARS + 1);
        let error = store.add(CoreFile::User, &too_long).unwrap_err();
        assert!(error.to_string().contains("cap"), "{error}");
        assert_eq!(store.core(CoreFile::User).unwrap(), "");
    }

    #[test]
    fn notes_are_written_read_and_ranked_with_recency() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(dir.path().to_path_buf());
        let now = at("2026-09-08T12:00:00Z");
        let old = store
            .note(
                NoteKind::Decision,
                "Postgres over MySQL",
                "chose postgres for the app database",
                "Because of jsonb and the team knows it.",
                at("2026-03-01T12:00:00Z"),
            )
            .unwrap();
        let fresh = store
            .note(
                NoteKind::Event,
                "Database migrated",
                "the app database moved to postgres 17",
                "Done on tenco.",
                at("2026-09-07T12:00:00Z"),
            )
            .unwrap();
        store
            .note(
                NoteKind::Risk,
                "Disk",
                "the disk is nearly full",
                "80% on /",
                now,
            )
            .unwrap();
        let notes = store.notes().unwrap();
        assert_eq!(notes.len(), 3);
        let hits = store.search("postgres database", 10, now).unwrap();
        let ids: Vec<&str> = hits.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids, [fresh.id.as_str(), old.id.as_str()], "{hits:?}");
        assert!(store.search("nothing here", 10, now).unwrap().is_empty());
        let got = store.get(&[old.id.clone(), "absent".into()]).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].body, "Because of jsonb and the team knows it.");
        let daily = store
            .daily(now, "handover", "compacted after a long day")
            .unwrap();
        let text = std::fs::read_to_string(daily).unwrap();
        assert!(text.starts_with("# 2026-09-08\n"), "{text}");
        assert!(text.contains("## 12:00 — handover"), "{text}");
    }
}
