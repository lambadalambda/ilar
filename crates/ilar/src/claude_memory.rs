//! Claude Code's auto memory, read into a project's ilar store, so a
//! folder Claude Code knows is not a stranger to ilar.
//!
//! Claude Code keeps `~/.claude/projects/<slug>/memory/`: one file per
//! fact with `name`, `description` and `metadata.type` in its
//! frontmatter, and a `MEMORY.md` of one pointer line per file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

use crate::memory::{MemoryStore, Note, NoteKind, common_git_dir, fit_headline, short_hash};

/// Where Claude Code keeps its state: `$CLAUDE_CONFIG_DIR`, else
/// `~/.claude`.
pub fn claude_home(config_dir: Option<PathBuf>, home: &Path) -> PathBuf {
    config_dir.unwrap_or_else(|| home.join(".claude"))
}

/// A project's directory name under `projects/`: the path with every
/// character outside `[A-Za-z0-9]` turned into `-`. Claude Code adds a
/// hash past 200 characters, which this does not; `--from` reaches
/// those.
pub fn slug(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The memory directories that may belong to `cwd`, most likely first.
/// Claude Code shares one across a repository's worktrees, under the
/// main checkout's path; an older one may sit under the launch
/// directory's own.
pub fn memory_dirs(claude_home: &Path, cwd: &Path) -> Vec<PathBuf> {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let main = common_git_dir(&cwd)
        .filter(|git| git.file_name().is_some_and(|name| name == ".git"))
        .and_then(|git| git.parent().map(Path::to_path_buf));
    let mut paths = Vec::new();
    for path in main.into_iter().chain([cwd]) {
        let dir = claude_home
            .join("projects")
            .join(slug(&path))
            .join("memory");
        if !paths.contains(&dir) {
            paths.push(dir);
        }
    }
    paths
}

/// One memory as Claude Code wrote it.
#[derive(Debug, Clone, PartialEq)]
struct ClaudeMemory {
    name: String,
    description: String,
    kind: NoteKind,
    modified: Option<DateTime<Utc>>,
    body: String,
}

/// What Claude Code's types are in ilar's kinds: who the person is and
/// how they want the work done are preferences; the project's facts
/// are decisions, which fade as slowly; a pointer elsewhere is a
/// solution.
fn kind_for(claude_type: &str) -> NoteKind {
    match claude_type {
        "user" | "feedback" => NoteKind::Preference,
        "reference" => NoteKind::Solution,
        _ => NoteKind::Decision,
    }
}

/// A memory file read back. The frontmatter is the few flat fields
/// Claude Code writes, read line by line; a file without any is named
/// by its file and summarised by its first line.
fn parse(stem: &str, text: &str) -> ClaudeMemory {
    let text = text.replace("\r\n", "\n");
    let (front, body) = text
        .strip_prefix("---\n")
        .and_then(|rest| {
            rest.split_once("\n---\n")
                .or_else(|| rest.strip_suffix("\n---").map(|front| (front, "")))
                .or_else(|| rest.strip_prefix("---\n").map(|body| ("", body)))
        })
        .unwrap_or(("", &text));
    let fields: HashMap<&str, &str> = front
        .lines()
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim(), unquote(value.trim())))
        .collect();
    let body = body.trim().to_string();
    let field = |key: &str| fields.get(key).filter(|value| !value.is_empty());
    ClaudeMemory {
        name: field("name").unwrap_or(&stem).to_string(),
        description: field("description")
            .map(|value| value.to_string())
            .unwrap_or_else(|| body.lines().next().unwrap_or_default().to_string()),
        kind: kind_for(field("type").unwrap_or(&"")),
        modified: field("modified")
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|when| when.with_timezone(&Utc)),
        body,
    }
}

fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(value)
}

/// `MEMORY.md`'s pointer lines as file name → title:
/// `- [Title](file.md) — hook`.
fn titles(index: &str) -> HashMap<String, String> {
    index
        .lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("- [")?;
            let (title, rest) = rest.split_once("](")?;
            let (file, _) = rest.split_once(')')?;
            Some((file.to_string(), title.to_string()))
        })
        .collect()
}

/// `tenco-install` as a title: `Tenco install`.
fn readable(name: &str) -> String {
    let spaced = name.replace(['-', '_'], " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => spaced,
    }
}

/// The id's tail, from the Claude name alone, so a memory imported
/// once is known again whatever its date has become since.
fn id_tail(name: &str) -> String {
    format!("{:08x}", short_hash(&format!("claude-code:{name}")))
}

fn to_note(memory: &ClaudeMemory, title: Option<&str>, when: DateTime<Utc>) -> Note {
    let title = title.map_or_else(|| readable(&memory.name), str::to_string);
    let body = format!(
        "{}\n\nImported from Claude Code's memory, where it was `{}`.",
        memory.body, memory.name
    );
    let (title, summary, body) = fit_headline(&title, &memory.description, &body);
    Note {
        id: format!("{}-{}", when.format("%Y%m%d"), id_tail(&memory.name)),
        kind: memory.kind.as_str().into(),
        title,
        summary,
        when,
        body,
    }
}

/// One memory file's fate in an [`import`].
#[derive(Debug, Clone)]
pub struct Imported {
    pub note: Note,
    /// False when an earlier import already brought it, whether it is
    /// still there, amended or forgotten.
    pub added: bool,
}

/// Every memory in `from` as a note in `store`, in file-name order.
/// One already imported is left alone: ilar may have amended it, or
/// forgotten it on purpose. A dry run writes nothing.
pub fn import(store: &MemoryStore, from: &Path, dry_run: bool) -> Result<Vec<Imported>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(from)
        .with_context(|| format!("reading {}", from.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|ext| ext == "md")
                && path.file_name().is_some_and(|name| name != "MEMORY.md")
        })
        .collect();
    files.sort();
    let titles = std::fs::read_to_string(from.join("MEMORY.md"))
        .map(|index| titles(&index))
        .unwrap_or_default();
    // The tails seen so far, this run's included: two files may carry
    // one name.
    let mut known: std::collections::HashSet<String> = store
        .ids_ever()
        .iter()
        .filter_map(|id| id.rsplit_once('-').map(|(_, tail)| tail.to_string()))
        .collect();
    let mut done = Vec::new();
    for path in files {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let stem = path.file_stem().unwrap_or_default().to_string_lossy();
        let memory = parse(&stem, &text);
        let file_name = path.file_name().unwrap_or_default().to_string_lossy();
        let when = memory.modified.unwrap_or_else(|| {
            std::fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .map(DateTime::<Utc>::from)
                .unwrap_or_else(|_| Utc::now())
        });
        let note = to_note(
            &memory,
            titles.get(file_name.as_ref()).map(String::as_str),
            when,
        );
        let added = known.insert(id_tail(&memory.name));
        if added && !dry_run {
            store.put(&note)?;
        }
        done.push(Imported { note, added });
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_types_become_kinds() {
        assert_eq!(kind_for("user"), NoteKind::Preference);
        assert_eq!(kind_for("feedback"), NoteKind::Preference);
        assert_eq!(kind_for("project"), NoteKind::Decision);
        assert_eq!(kind_for("reference"), NoteKind::Solution);
        assert_eq!(kind_for(""), NoteKind::Decision);
    }

    #[test]
    fn the_index_names_each_file() {
        let titles =
            titles("# Memory index\n\n- [Use the Edit tool](use-the-edit-tool.md) — hook\nprose\n");
        assert_eq!(titles.len(), 1);
        assert_eq!(titles["use-the-edit-tool.md"], "Use the Edit tool");
    }

    #[test]
    fn a_top_level_type_and_an_empty_name_are_read() {
        let memory = parse("stem", "---\nname:\ntype: reference\n---\nBody line\n");
        assert_eq!(memory.name, "stem");
        assert_eq!(memory.kind, NoteKind::Solution);
        assert_eq!(memory.description, "Body line");
    }

    #[test]
    fn crlf_and_a_closing_line_at_the_end_are_frontmatter() {
        let memory = parse("stem", "---\r\nname: crlf\r\n---\r\n\r\nBody\r\n");
        assert_eq!(
            (memory.name.as_str(), memory.body.as_str()),
            ("crlf", "Body")
        );
        let memory = parse("stem", "---\nname: bare\ndescription: d\n---");
        assert_eq!((memory.name.as_str(), memory.body.as_str()), ("bare", ""));
        let memory = parse("stem", "---\n---\nBody\n");
        assert_eq!(
            (memory.name.as_str(), memory.description.as_str()),
            ("stem", "Body")
        );
    }
}
