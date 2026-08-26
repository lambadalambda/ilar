//! glob: file pattern matching (e.g. src/**/*.rs).

use serde::Deserialize;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::{
    Tool, ToolConcurrency, ToolContext, ToolFuture, ToolOutput, WorkspaceAccess, parse_input,
};

const MAX_MATCHES: usize = 1000;
/// Entries visited before the walk gives up. Bounds the pathological
/// case (an unfiltered monorepo) to seconds instead of an apparent hang.
const MAX_ENTRIES: usize = 500_000;
const MAX_THREADS: usize = 8;

pub struct GlobTool;

#[derive(Deserialize)]
struct Input {
    pattern: String,
    /// Include gitignored paths (build output, `.env`). Off by default.
    #[serde(default)]
    include_ignored: bool,
}

#[derive(Clone, Copy)]
struct Limits {
    max_matches: usize,
    max_entries: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_matches: MAX_MATCHES,
            max_entries: MAX_ENTRIES,
        }
    }
}

/// Leading path components that contain no glob metacharacters. Walking
/// can start there instead of the workspace root, which is the whole
/// difference between opening one directory and enumerating a monorepo.
///
/// A root or `..` is kept, not dropped: joined onto cwd it is exactly
/// where the caller pointed — an absolute prefix replaces the base, a
/// `..` climbs out of it.
fn literal_prefix(pattern: &str) -> PathBuf {
    let path = Path::new(pattern);
    let mut prefix = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => continue,
            Component::Prefix(_) | Component::RootDir | Component::ParentDir => {
                prefix.push(component.as_os_str());
            }
            Component::Normal(part) => {
                if part
                    .to_string_lossy()
                    .contains(['*', '?', '[', ']', '{', '}'])
                {
                    break;
                }
                prefix.push(part);
            }
        }
    }
    prefix
}

/// A runaway like `{a,b}{a,b}{a,b}...` multiplies; far above anything a
/// model writes by hand, far below anything that hurts.
const MAX_BRACE_EXPANSIONS: usize = 64;

/// Expand `{a,b}` alternation, which `glob::Pattern` lacks: without
/// this, `**/{route,client}/*.ts` pays for the whole walk and then
/// reports "(no matches)" because nothing contains a literal brace.
/// Nested groups expand recursively; a brace inside a `[...]` class
/// stays literal.
fn expand_braces(pattern: &str) -> Result<Vec<String>, String> {
    /// Byte range of the first top-level group, if any.
    fn first_group(pattern: &str) -> Result<Option<(usize, usize)>, String> {
        let bytes = pattern.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            match bytes[index] {
                b'[' => {
                    // Skip the class: `[!]x]` may open with a negation
                    // and a literal `]` before the closing one.
                    index += 1;
                    if bytes.get(index) == Some(&b'!') {
                        index += 1;
                    }
                    if bytes.get(index) == Some(&b']') {
                        index += 1;
                    }
                    while index < bytes.len() && bytes[index] != b']' {
                        index += 1;
                    }
                }
                b'{' => {
                    let open = index;
                    let mut depth = 1usize;
                    index += 1;
                    while index < bytes.len() && depth > 0 {
                        match bytes[index] {
                            b'{' => depth += 1,
                            b'}' => depth -= 1,
                            _ => {}
                        }
                        index += 1;
                    }
                    if depth > 0 {
                        return Err("unbalanced braces in pattern".into());
                    }
                    return Ok(Some((open, index - 1)));
                }
                _ => {}
            }
            index += 1;
        }
        Ok(None)
    }

    let Some((open, close)) = first_group(pattern)? else {
        return Ok(vec![pattern.to_string()]);
    };
    let prefix = &pattern[..open];
    let suffix = &pattern[close + 1..];
    let body = &pattern[open + 1..close];
    let mut alternatives = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (index, byte) in body.bytes().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            b',' if depth == 0 => {
                alternatives.push(&body[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    alternatives.push(&body[start..]);
    let mut expanded = Vec::new();
    for alternative in alternatives {
        expanded.extend(expand_braces(&format!("{prefix}{alternative}{suffix}"))?);
        if expanded.len() > MAX_BRACE_EXPANSIONS {
            return Err(format!(
                "brace expansion exceeds {MAX_BRACE_EXPANSIONS} patterns"
            ));
        }
    }
    Ok(expanded)
}

fn scan(
    cwd: &Path,
    pattern: &str,
    include_ignored: bool,
    limits: Limits,
    cancelled: &AtomicBool,
) -> ToolOutput {
    let expanded = match expand_braces(pattern) {
        Ok(expanded) => expanded,
        Err(error) => return ToolOutput::error(format!("glob: {error}")),
    };
    let compiled = match expanded
        .iter()
        .map(|pattern| glob::Pattern::new(pattern))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(compiled) => compiled,
        Err(error) => return ToolOutput::error(format!("glob: invalid pattern: {error}")),
    };
    let options = glob::MatchOptions {
        case_sensitive: true,
        require_literal_separator: true,
        require_literal_leading_dot: false,
    };

    // `Path::join` replaces the base for an absolute prefix, which is
    // the behaviour: an absolute pattern is matched against absolute
    // paths and reported as such, a relative one against paths relative
    // to cwd. Same semantics as read/write/edit.
    let absolute = Path::new(pattern).is_absolute();
    let root = cwd.join(literal_prefix(pattern));
    let threads = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
        .min(MAX_THREADS);
    // Hidden entries stay visible so `.github/workflows/*.yml` keeps
    // working; the ignore files do the heavy filtering, and `.git` is
    // dropped explicitly because nothing there is a source file.
    let walker = ignore::WalkBuilder::new(&root)
        .hidden(false)
        .ignore(!include_ignored)
        .git_ignore(!include_ignored)
        .git_global(!include_ignored)
        .git_exclude(!include_ignored)
        .parents(!include_ignored)
        // Honour ignore files even outside a git repository.
        .require_git(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .threads(threads)
        .build_parallel();

    let matches = Mutex::new(Vec::new());
    let scanned = AtomicUsize::new(0);
    let capped_matches = AtomicBool::new(false);
    let capped_entries = AtomicBool::new(false);

    walker.run(|| {
        Box::new(|entry| {
            if cancelled.load(Ordering::Acquire) {
                return ignore::WalkState::Quit;
            }
            if scanned.fetch_add(1, Ordering::Relaxed) >= limits.max_entries {
                capped_entries.store(true, Ordering::Release);
                return ignore::WalkState::Quit;
            }
            let Ok(entry) = entry else {
                return ignore::WalkState::Continue;
            };
            let candidate = if absolute {
                entry.path()
            } else {
                match entry.path().strip_prefix(cwd) {
                    Ok(relative) => relative,
                    Err(_) => return ignore::WalkState::Continue,
                }
            };
            if candidate.as_os_str().is_empty()
                || !compiled
                    .iter()
                    .any(|pattern| pattern.matches_path_with(candidate, options))
            {
                return ignore::WalkState::Continue;
            }
            let mut matches = matches.lock().unwrap();
            if matches.len() >= limits.max_matches {
                capped_matches.store(true, Ordering::Release);
                return ignore::WalkState::Quit;
            }
            matches.push(candidate.to_string_lossy().into_owned());
            ignore::WalkState::Continue
        })
    });

    if cancelled.load(Ordering::Acquire) {
        return ToolOutput::error("cancelled");
    }
    let mut matches = matches.into_inner().unwrap();
    matches.sort();
    matches.truncate(limits.max_matches);
    if capped_matches.load(Ordering::Acquire) {
        matches.push(format!("…(truncated at {} matches)", limits.max_matches));
    } else if capped_entries.load(Ordering::Acquire) {
        matches.push(format!(
            "…(truncated: scanned {} paths without finishing; narrow the pattern)",
            limits.max_entries
        ));
    }
    if matches.is_empty() {
        ToolOutput::text("(no matches)")
    } else {
        ToolOutput::text(matches.join("\n"))
    }
}

impl Tool for GlobTool {
    fn name(&self) -> &'static str {
        "glob"
    }

    fn description(&self) -> &'static str {
        "Find files by glob pattern (e.g. src/**/*.{rs,toml}), relative to cwd \
         or absolute (/tmp/**/*.txt). Supports {a,b} alternation. \
         Gitignored paths are skipped unless include_ignored is set."
    }

    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Concurrent
    }
    fn workspace_access(&self) -> WorkspaceAccess {
        WorkspaceAccess::ReadOnly
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern, relative to cwd or absolute"
                },
                "include_ignored": {
                    "type": "boolean",
                    "description": "Include gitignored paths (default false)"
                }
            },
            "required": ["pattern"]
        })
    }

    fn run(&self, input: serde_json::Value, ctx: ToolContext) -> ToolFuture {
        Box::pin(async move {
            let input: Input = match parse_input(input, "glob") {
                Ok(v) => v,
                Err(e) => return e,
            };
            match super::blocking_scan(move |cancelled| {
                scan(
                    &ctx.cwd,
                    &input.pattern,
                    input.include_ignored,
                    Limits::default(),
                    &cancelled,
                )
            })
            .await
            {
                Ok(output) => output,
                Err(error) => ToolOutput::error(format!("glob worker failed: {error}")),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_prefix_stops_at_the_first_metacharacter() {
        assert_eq!(literal_prefix("src/**/*.rs"), Path::new("src"));
        assert_eq!(
            literal_prefix("worktrees/manteca/*"),
            Path::new("worktrees/manteca")
        );
        assert_eq!(literal_prefix("*.txt"), Path::new(""));
        assert_eq!(literal_prefix("**/foo"), Path::new(""));
        assert_eq!(literal_prefix("src/a[0-9]/b"), Path::new("src"));
        assert_eq!(literal_prefix("src/main.rs"), Path::new("src/main.rs"));
        assert_eq!(literal_prefix("./src/*.rs"), Path::new("src"));
        assert_eq!(literal_prefix("src/{a,b}/x"), Path::new("src"));
    }

    /// A prefix that leaves cwd is kept whole: joined onto cwd it is the
    /// directory the caller asked for, which is where the walk starts.
    #[test]
    fn literal_prefix_keeps_roots_and_parents() {
        assert_eq!(literal_prefix("/etc/*"), Path::new("/etc"));
        assert_eq!(
            literal_prefix("/tmp/spill/x.txt"),
            Path::new("/tmp/spill/x.txt")
        );
        assert_eq!(literal_prefix("../*.rs"), Path::new(".."));
        assert_eq!(literal_prefix("src/../../*.rs"), Path::new("src/../.."));
    }

    /// An absolute pattern matches (and reports) absolute paths, so a
    /// spill file in the state dir is reachable from any cwd.
    #[test]
    fn scan_matches_absolute_patterns_outside_cwd() {
        let cwd = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let spill = std::fs::canonicalize(elsewhere.path())
            .unwrap()
            .join("call-1.txt");
        std::fs::write(&spill, "").unwrap();
        let cancelled = AtomicBool::new(false);

        let out = scan(
            cwd.path(),
            &format!("{}/*.txt", spill.parent().unwrap().display()),
            false,
            Limits::default(),
            &cancelled,
        );

        assert!(!out.is_error, "{}", out.content);
        assert_eq!(out.content, spill.to_string_lossy());
    }

    /// A relative pattern that climbs out of cwd stays relative, both in
    /// what it matches and in what it reports.
    #[test]
    fn scan_matches_patterns_that_climb_out_of_cwd() {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("nested");
        std::fs::create_dir(&cwd).unwrap();
        std::fs::write(root.path().join("sibling.txt"), "").unwrap();
        let cancelled = AtomicBool::new(false);

        let out = scan(&cwd, "../*.txt", false, Limits::default(), &cancelled);

        assert!(!out.is_error, "{}", out.content);
        assert_eq!(out.content, "../sibling.txt");
    }

    #[test]
    fn brace_alternation_expands_to_plain_patterns() {
        assert_eq!(
            expand_braces("**/{route,client}/*.ts").unwrap(),
            vec!["**/route/*.ts", "**/client/*.ts"]
        );
        assert_eq!(expand_braces("*.{ts,tsx}").unwrap(), vec!["*.ts", "*.tsx"]);
        assert_eq!(expand_braces("{a,b{c,d}}").unwrap(), vec!["a", "bc", "bd"]);
        assert_eq!(
            expand_braces("no/braces/*.rs").unwrap(),
            vec!["no/braces/*.rs"]
        );
        // A brace inside a character class is literal, not a group.
        assert_eq!(expand_braces("a[{]b").unwrap(), vec!["a[{]b"]);
        assert!(expand_braces("src/{a,b").is_err());
    }

    #[test]
    fn scan_matches_brace_alternation() {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["route", "client", "other"] {
            std::fs::create_dir(dir.path().join(sub)).unwrap();
            std::fs::write(dir.path().join(sub).join("x.ts"), "").unwrap();
        }
        let cancelled = AtomicBool::new(false);
        let out = scan(
            dir.path(),
            "{route,client}/*.ts",
            false,
            Limits::default(),
            &cancelled,
        );
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(out.content, "client/x.ts\nroute/x.ts");

        let out = scan(dir.path(), "{route,", false, Limits::default(), &cancelled);
        assert!(out.is_error);
        assert!(out.content.contains("unbalanced"), "{}", out.content);
    }

    #[test]
    fn scan_reports_the_entry_budget_separately_from_the_match_cap() {
        let dir = tempfile::tempdir().unwrap();
        for index in 0..20 {
            std::fs::write(dir.path().join(format!("f{index}.txt")), "").unwrap();
        }
        let cancelled = AtomicBool::new(false);
        let out = scan(
            dir.path(),
            "*.txt",
            false,
            Limits {
                max_matches: MAX_MATCHES,
                max_entries: 5,
            },
            &cancelled,
        );
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("scanned 5 paths"), "{}", out.content);
    }
}
