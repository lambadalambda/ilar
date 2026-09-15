//! grep: regex search across files, gitignore-aware, file:line:match.

use serde::Deserialize;
use std::io::{BufRead, Read as _};
use std::sync::atomic::Ordering;

use super::{
    Tool, ToolConcurrency, ToolContext, ToolFuture, ToolOutput, WorkspaceAccess, parse_input,
};
use crate::text::truncate_bytes_ellipsis;

const MAX_MATCHES: usize = 200;
const MAX_MATCHES_PER_FILE: usize = 50;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_OUTPUT_LINE_BYTES: usize = 8 * 1024;
/// Files visited before the walk gives up. Bounds a rare-match search
/// over a monorepo, which the match cap cannot short-circuit.
const MAX_ENTRIES: usize = 500_000;
const MAX_THREADS: usize = 8;
/// Context lines either side of a match, at most. rg's `-C` in the
/// weekend logs never went past this.
const MAX_CONTEXT: usize = 10;

pub struct GrepTool;

#[derive(Deserialize)]
struct Input {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    /// Search gitignored files too. Off by default.
    #[serde(default)]
    include_ignored: bool,
    /// Lines of context before and after each match.
    #[serde(default)]
    context: Option<usize>,
    #[serde(default)]
    ignore_case: bool,
    /// Only search files matching this glob.
    #[serde(default)]
    glob: Option<String>,
    /// Stop after this many matches.
    #[serde(default)]
    limit: Option<usize>,
}

/// One rendered line — a `path:line:text` match or a `path-line-text`
/// context line — kept with its sort key so parallel walking cannot
/// reorder the output.
struct Hit {
    path: String,
    line: usize,
    is_match: bool,
    rendered: String,
}

/// Everything a scan needs besides the file: shared across the walk's
/// threads.
struct Search {
    regex: regex::Regex,
    context: usize,
    files: Option<FileFilter>,
}

/// The `glob` input: a pattern without `/` is matched against the file
/// name at any depth, one with `/` against the path relative to cwd —
/// rg's rule, and the one a model writing `*.rs` expects.
struct FileFilter {
    patterns: Vec<glob::Pattern>,
    against_path: bool,
}

impl FileFilter {
    fn parse(pattern: &str) -> Result<Self, String> {
        // `./src/*.rs` is how a model spells "from here"; the walk's
        // relative paths never start that way.
        let pattern = pattern.strip_prefix("./").unwrap_or(pattern);
        Ok(Self {
            patterns: super::glob::compile_pattern(pattern)
                .map_err(|error| format!("glob: {error}"))?,
            against_path: pattern.contains('/'),
        })
    }

    fn admits(&self, relative: &str, file_name: &str) -> bool {
        let options = glob::MatchOptions {
            case_sensitive: true,
            require_literal_separator: self.against_path,
            require_literal_leading_dot: false,
        };
        let subject = if self.against_path {
            relative
        } else {
            file_name
        };
        self.patterns
            .iter()
            .any(|pattern| pattern.matches_with(subject, options))
    }
}

impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> &'static str {
        "Search file contents with a regex, recursively from cwd (or path, \
         which may be relative to cwd or absolute). \
         Gitignored files are skipped unless include_ignored is set. \
         Returns file:line:match; with context, surrounding lines as \
         file-line-text and -- between groups. Narrow with glob (*.rs, \
         src/**/*.ts) and cap with limit instead of piping through head. \
         Beyond limit it caps at 50 matches per file, the first 2 MiB of \
         each file and 256 KiB of output; the closing line says which \
         cap bit."
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
                "pattern": {"type": "string", "description": "Rust regex"},
                "path": {"type": "string", "description": "File or directory to search, relative to cwd or absolute (default: cwd)"},
                "include_ignored": {
                    "type": "boolean",
                    "description": "Search gitignored files too (default false)"
                },
                "context": {
                    "type": "integer",
                    "description": "Lines of context before and after each match (0–10, default 0)"
                },
                "ignore_case": {"type": "boolean", "description": "Case-insensitive match (default false)"},
                "glob": {
                    "type": "string",
                    "description": "Only search files matching this glob: a bare name pattern (*.rs, *.{js,ts}) at any depth, or a path (src/**/*.rs) relative to cwd"
                },
                "limit": {
                    "type": "integer",
                    "description": "Stop after this many matches (default and maximum 200): the first found, then sorted by path"
                }
            },
            "required": ["pattern"]
        })
    }

    fn run(&self, input: serde_json::Value, ctx: ToolContext) -> ToolFuture {
        Box::pin(async move {
            let input: Input = match parse_input(input, "grep") {
                Ok(v) => v,
                Err(e) => return e,
            };
            // Same path semantics as read/write/edit: `Path::join`
            // replaces the base when the requested path is absolute, so
            // an absolute path stands and a relative one resolves from
            // cwd. Searching outside the workspace is allowed on
            // purpose — spilled tool output lives in the state dir.
            let requested = input.path.as_deref().unwrap_or(".");
            let root = ctx.cwd.join(requested);
            if !root.exists() {
                return ToolOutput::error(format!(
                    "grep: no such path {requested}; find the right one with glob"
                ));
            }
            let spill = super::bash::SpillTarget::from_context(&ctx);
            let regex = match regex::RegexBuilder::new(&input.pattern)
                .case_insensitive(input.ignore_case)
                .build()
            {
                Ok(regex) => regex,
                Err(error) => return ToolOutput::error(format!("grep: invalid regex: {error}")),
            };
            let files = match input.glob.as_deref().map(FileFilter::parse) {
                Some(Ok(filter)) => Some(filter),
                Some(Err(error)) => return ToolOutput::error(format!("grep: {error}")),
                None => None,
            };
            let search = Search {
                regex,
                context: input.context.unwrap_or(0).min(MAX_CONTEXT),
                files,
            };
            let limit = input.limit.unwrap_or(MAX_MATCHES).clamp(1, MAX_MATCHES);
            match super::blocking_scan(move |cancelled| {
                grep_files(
                    &ctx.cwd,
                    &root,
                    &search,
                    input.include_ignored,
                    limit,
                    MAX_ENTRIES,
                    &cancelled,
                )
            })
            .await
            {
                Ok(output) => spill_over_budget(output, spill.as_ref()).await,
                Err(error) => ToolOutput::error(format!("grep: worker failed: {error}")),
            }
        })
    }
}

/// Why one file's scan stopped early. A bare "(truncated)" covered
/// three different caps and named none of them, so nobody could tell a
/// pattern that needs narrowing from a file too big to read.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Clip {
    PerFileMatches,
    FileBytes,
}

/// Scan one file. Pure apart from reading it: returns the hits and
/// which cap, if any, clipped the scan.
fn grep_one_file(
    path: &std::path::Path,
    relative: &str,
    search: &Search,
    cancelled: &std::sync::atomic::AtomicBool,
) -> (Vec<Hit>, Option<Clip>) {
    let Ok(file) = std::fs::File::open(path) else {
        return (Vec::new(), None);
    };
    let mut reader = std::io::BufReader::new(file).take(MAX_FILE_BYTES + 1);
    let mut hits = Vec::new();
    let mut matches = 0_usize;
    let mut truncated = None;
    let mut line = Vec::new();
    let mut line_number = 0_usize;
    let mut file_bytes = 0_u64;
    // Context bookkeeping: the last `context` unemitted lines wait in
    // `before`; a match emits them, itself, and then the next `after`
    // lines as they come. `last_emitted` keeps overlapping windows
    // from repeating a line.
    let mut before: std::collections::VecDeque<(usize, String)> =
        std::collections::VecDeque::with_capacity(search.context);
    let mut after = 0_usize;
    let mut last_emitted = 0_usize;
    loop {
        if cancelled.load(Ordering::Acquire) {
            return (hits, truncated);
        }
        line.clear();
        let Ok(read) = reader.read_until(b'\n', &mut line) else {
            break;
        };
        if read == 0 {
            break;
        }
        let previous_file_bytes = file_bytes;
        file_bytes += read as u64;
        let file_limit_reached = file_bytes > MAX_FILE_BYTES;
        if file_limit_reached {
            let remaining = MAX_FILE_BYTES.saturating_sub(previous_file_bytes) as usize;
            truncated = Some(Clip::FileBytes);
            if remaining == 0 {
                break;
            }
            line.truncate(remaining);
        }
        if line.last() == Some(&b'\n') {
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
        }
        line_number += 1;
        let text = String::from_utf8_lossy(&line);
        if search.regex.is_match(&text) {
            for (number, kept) in before.drain(..) {
                if number > last_emitted {
                    hits.push(hit(relative, number, &kept, false));
                }
            }
            hits.push(hit(relative, line_number, &text, true));
            last_emitted = line_number;
            after = search.context;
            matches += 1;
            if matches >= MAX_MATCHES_PER_FILE {
                truncated = Some(Clip::PerFileMatches);
                break;
            }
        } else if after > 0 {
            hits.push(hit(relative, line_number, &text, false));
            last_emitted = line_number;
            after -= 1;
        } else if search.context > 0 {
            if before.len() == search.context {
                before.pop_front();
            }
            before.push_back((line_number, text.into_owned()));
        }
        if file_limit_reached {
            break;
        }
    }
    (hits, truncated)
}

/// rg's spelling: `:` around a match's line number, `-` around a
/// context line's.
fn hit(relative: &str, line: usize, text: &str, is_match: bool) -> Hit {
    let sep = if is_match { ':' } else { '-' };
    let mut rendered = format!("{relative}{sep}{line}{sep}{}", text.trim_end());
    truncate_bytes_ellipsis(&mut rendered, MAX_OUTPUT_LINE_BYTES);
    Hit {
        path: relative.to_string(),
        line,
        is_match,
        rendered,
    }
}

fn grep_files(
    cwd: &std::path::Path,
    root: &std::path::Path,
    search: &Search,
    include_ignored: bool,
    limit: usize,
    max_entries: usize,
    cancelled: &std::sync::atomic::AtomicBool,
) -> ToolOutput {
    let threads = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
        .min(MAX_THREADS);
    // Matches glob: dotted paths stay searchable (`.github/**`, `.env`),
    // ignore files are honoured even outside a git repo, `.git` is out.
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .ignore(!include_ignored)
        .git_ignore(!include_ignored)
        .git_global(!include_ignored)
        .git_exclude(!include_ignored)
        .parents(!include_ignored)
        .require_git(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .threads(threads)
        .build_parallel();

    let hits = std::sync::Mutex::new(Vec::<Hit>::new());
    let matched = std::sync::atomic::AtomicUsize::new(0);
    let scanned = std::sync::atomic::AtomicUsize::new(0);
    let clipped_matches = std::sync::atomic::AtomicBool::new(false);
    let clipped_bytes = std::sync::atomic::AtomicBool::new(false);
    let capped_entries = std::sync::atomic::AtomicBool::new(false);

    walker.run(|| {
        Box::new(|entry| {
            if cancelled.load(Ordering::Acquire) {
                return ignore::WalkState::Quit;
            }
            let Ok(entry) = entry else {
                return ignore::WalkState::Continue;
            };
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                return ignore::WalkState::Continue;
            }
            if scanned.fetch_add(1, Ordering::Relaxed) >= max_entries {
                capped_entries.store(true, Ordering::Release);
                return ignore::WalkState::Quit;
            }
            if matched.load(Ordering::Acquire) >= limit {
                return ignore::WalkState::Quit;
            }
            let relative = entry
                .path()
                .strip_prefix(cwd)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .into_owned();
            if let Some(files) = &search.files
                && !files.admits(&relative, &entry.file_name().to_string_lossy())
            {
                return ignore::WalkState::Continue;
            }
            let (found, file_clipped) = grep_one_file(entry.path(), &relative, search, cancelled);
            match file_clipped {
                Some(Clip::PerFileMatches) => clipped_matches.store(true, Ordering::Release),
                Some(Clip::FileBytes) => clipped_bytes.store(true, Ordering::Release),
                None => {}
            }
            if !found.is_empty() {
                let found_matches = found.iter().filter(|hit| hit.is_match).count();
                matched.fetch_add(found_matches, Ordering::AcqRel);
                hits.lock().unwrap().extend(found);
            }
            ignore::WalkState::Continue
        })
    });

    if cancelled.load(Ordering::Acquire) {
        return ToolOutput::error("grep: cancelled");
    }
    let mut hits = hits.into_inner().unwrap();
    // Parallel walking loses walk order; make the output reproducible.
    hits.sort_by(|left, right| left.path.cmp(&right.path).then(left.line.cmp(&right.line)));
    let mut output_clipped = false;
    // The walk stops once the cap is met, so whether a match past it
    // was ever seen is timing; what is known is that the cap was
    // reached, and that is what the notice says.
    let limit_reached = hits.iter().filter(|hit| hit.is_match).count() >= limit;
    // The cap counts matches; a match's own context rides along with it,
    // and everything past the last admitted match is dropped — including
    // the context that was leading up to the next one.
    let mut seen_matches = 0_usize;
    let mut kept = hits
        .iter()
        .position(|hit| {
            if hit.is_match {
                seen_matches += 1;
            }
            seen_matches > limit
        })
        .unwrap_or(hits.len());
    if kept < hits.len() {
        while kept > 0 && !hits[kept - 1].is_match && hits[kept - 1].line + 1 == hits[kept].line {
            kept -= 1;
        }
        hits.truncate(kept);
    }
    let mut out = String::new();
    let mut previous: Option<(&str, usize)> = None;
    for hit in &hits {
        // rg's group separator: only meaningful when context makes
        // groups, and only where the next line is not adjacent.
        let separated = search.context > 0
            && previous.is_some_and(|(path, line)| path != hit.path || hit.line > line + 1);
        let needed = hit.rendered.len() + 1 + if separated { 3 } else { 0 };
        if out.len().saturating_add(needed) > MAX_OUTPUT_BYTES {
            output_clipped = true;
            break;
        }
        if separated {
            out.push_str("--\n");
        }
        out.push_str(&hit.rendered);
        out.push('\n');
        previous = Some((&hit.path, hit.line));
    }
    if capped_entries.load(Ordering::Acquire) {
        close_with(
            &mut out,
            &format!(
                "…(truncated: scanned {max_entries} files without finishing; narrow the path)"
            ),
            MAX_OUTPUT_BYTES,
        );
    } else if limit_reached {
        close_with(
            &mut out,
            &format!("…(limit {limit} reached; raise limit or narrow the pattern)"),
            MAX_OUTPUT_BYTES,
        );
    } else if let Some(notice) = cap_notice(
        clipped_matches.load(Ordering::Acquire),
        clipped_bytes.load(Ordering::Acquire),
        output_clipped,
    ) {
        close_with(&mut out, &notice, MAX_OUTPUT_BYTES);
    }
    ToolOutput::text(out)
}

/// The closing line for whatever cap bit, naming it: three different
/// caps used to come back as the same bare "(truncated)", and the fix
/// for each of them is a different one.
fn cap_notice(per_file_matches: bool, file_bytes: bool, output: bool) -> Option<String> {
    let causes: Vec<String> = [
        per_file_matches.then(|| format!("{MAX_MATCHES_PER_FILE} matches per file")),
        file_bytes.then(|| {
            format!(
                "the first {} of a file",
                crate::text::format_bytes(MAX_FILE_BYTES)
            )
        }),
        output.then(|| {
            format!(
                "{} of output",
                crate::text::format_bytes(MAX_OUTPUT_BYTES as u64)
            )
        }),
    ]
    .into_iter()
    .flatten()
    .collect();
    (!causes.is_empty()).then(|| {
        format!(
            "…(truncated at {}; narrow the pattern or the path)",
            causes.join(" and ")
        )
    })
}

/// Append a closing notice within `limit`, always on a line of its own.
/// The byte cut lands mid-line whenever the last hit straddles it, and a
/// notice glued to half a match is one no reader — and no carry-forward
/// filter — can find.
fn close_with(out: &mut String, notice: &str, limit: usize) {
    truncate_bytes_ellipsis(out, limit.saturating_sub(notice.len() + 1));
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(notice);
    out.push('\n');
}

/// Past the bash preview budget the full match list goes to disk and
/// the result opens with the pointer — same discipline, same directory,
/// same sweep. Head-biased, unlike bash: matches are sorted by path, so
/// the front is where the grouping starts.
async fn spill_over_budget(
    output: ToolOutput,
    spill: Option<&super::bash::SpillTarget>,
) -> ToolOutput {
    let Some(target) = spill else {
        return output;
    };
    if output.is_error || output.content.len() <= super::bash::MAX_PREVIEW {
        return output;
    }
    let full = output.content;
    let saved = target
        .write_note(full.as_bytes(), "grep or read it for what you need")
        .await;
    // The closing notice (match cap, entry budget) must not vanish into
    // the file: carry it past the cut. `close_with` guarantees it is a
    // line of its own, so this finds it whatever the byte cut did.
    let closing = full
        .lines()
        .next_back()
        .filter(|line| line.starts_with('…'))
        .map(str::to_string);
    let mut head = full;
    truncate_bytes_ellipsis(&mut head, super::bash::MAX_PREVIEW);
    if let Some(cut) = head.rfind('\n') {
        head.truncate(cut + 1);
    }
    // "every match in this result", not "every match": what was spilled
    // is grep's own already-capped output (200 matches, 50 per file,
    // 256 KiB), and the carried notice below says when that bit.
    head.push_str(match &saved {
        Ok(_) => "…(the preview ends here; the file has every match in this result)\n",
        Err(_) => "…(the preview ends here; the rest could not be saved)\n",
    });
    if let Some(closing) = closing {
        head.push_str(&closing);
        head.push('\n');
    }
    let note = saved.unwrap_or_else(|failure| failure);
    ToolOutput::text(format!("{note}\n{head}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One "(truncated)" covered three caps, and each of them has a
    /// different fix — so the notice names the one that bit.
    #[test]
    fn the_closing_notice_names_the_cap_that_bit() {
        assert_eq!(cap_notice(false, false, false), None);
        let matches = cap_notice(true, false, false).unwrap();
        assert!(matches.contains("50 matches per file"), "{matches}");
        let bytes = cap_notice(false, true, false).unwrap();
        assert!(bytes.contains("the first 2.0 MiB of a file"), "{bytes}");
        let output = cap_notice(false, false, true).unwrap();
        assert!(output.contains("256.0 KiB of output"), "{output}");
        // More than one can bite in the same search.
        let both = cap_notice(true, false, true).unwrap();
        assert!(both.contains("matches per file and"), "{both}");
        assert!(both.contains("narrow the pattern"), "{both}");
    }

    #[tokio::test]
    async fn a_missing_path_is_an_error_not_an_empty_result() {
        let dir = tempfile::tempdir().unwrap();
        let out = GrepTool
            .run(
                serde_json::json!({"pattern": "x", "path": "src/nope"}),
                ToolContext::root(dir.path().to_path_buf()),
            )
            .await;
        assert!(out.is_error, "{}", out.content);
        assert!(
            out.content.contains("no such path src/nope"),
            "{}",
            out.content
        );
    }

    /// A search with none of the options: what the pre-option tool did.
    fn plain(pattern: &str) -> Search {
        Search {
            regex: regex::Regex::new(pattern).unwrap(),
            context: 0,
            files: None,
        }
    }

    #[tokio::test]
    async fn matches_past_the_preview_budget_spill_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        // 240 matches of ~300 chars: past the match cap AND past the
        // preview budget once the surviving 200 are rendered.
        let line = format!("needle {}\n", "x".repeat(290));
        for index in 0..4 {
            std::fs::write(dir.path().join(format!("f{index}.txt")), line.repeat(60)).unwrap();
        }
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let full = grep_files(
            dir.path(),
            dir.path(),
            &plain("needle"),
            false,
            MAX_MATCHES,
            MAX_ENTRIES,
            &cancelled,
        );
        assert!(full.content.len() > super::super::bash::MAX_PREVIEW);

        let spill_dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::root(dir.path().to_path_buf())
            .with_spill_dir(spill_dir.path().to_path_buf());
        let target = super::super::bash::SpillTarget::from_context(&ctx);
        let out = spill_over_budget(full.clone(), target.as_ref()).await;

        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.starts_with("full output: "), "{}", out.content);
        assert!(
            out.content.len() < super::super::bash::MAX_PREVIEW + 1024,
            "preview stayed near the budget: {} bytes",
            out.content.len()
        );
        // What was spilled is grep's own capped rendering, not the
        // repository's every match; the claim says which.
        assert!(
            out.content
                .contains("the file has every match in this result"),
            "{}",
            out.content
        );
        // The match-cap notice survives the cut.
        assert!(
            out.content
                .trim_end()
                .ends_with("raise limit or narrow the pattern)"),
            "{}",
            out.content
        );
        // The file holds the full rendering, byte for byte.
        let file = std::fs::read_dir(spill_dir.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(std::fs::read_to_string(file).unwrap(), full.content);
    }

    #[tokio::test]
    async fn matches_within_the_budget_never_spill() {
        let spill_dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::root(std::path::PathBuf::from("."))
            .with_spill_dir(spill_dir.path().to_path_buf());
        let target = super::super::bash::SpillTarget::from_context(&ctx);
        let small = ToolOutput::text("a.txt:1:hit\n".to_string());
        let out = spill_over_budget(small.clone(), target.as_ref()).await;
        assert_eq!(out.content, small.content);
        assert!(
            std::fs::read_dir(spill_dir.path())
                .unwrap()
                .next()
                .is_none()
        );
    }

    /// The byte budget cuts wherever the last hit straddles it — often
    /// mid-line. A closing notice glued onto half a match is invisible
    /// to a reader and to the carry-forward filter in
    /// [`spill_over_budget`] alike, so it always gets its own line.
    #[test]
    fn a_closing_notice_survives_a_cut_that_lands_mid_line() {
        let mut out = "src/a.rs:1:a long match line that the budget cuts through\n".to_string();
        close_with(&mut out, "…(truncated)", 30);

        assert_eq!(
            out.lines().next_back(),
            Some("…(truncated)"),
            "the notice fused with a partial hit: {out:?}"
        );
    }

    /// The carried notice reaches the model: it is the only thing that
    /// says the *result* was capped, which the spill file cannot.
    #[tokio::test]
    async fn a_mid_line_cut_still_carries_its_cap_notice_past_the_spill() {
        let mut full = "src/a.rs:1:x\n".repeat(super::super::bash::MAX_PREVIEW / 12 + 200);
        let mid_line = full.len() - 5;
        close_with(&mut full, "…(truncated)", mid_line);
        assert_eq!(full.lines().next_back(), Some("…(truncated)"));

        let spill_dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::root(std::path::PathBuf::from("."))
            .with_spill_dir(spill_dir.path().to_path_buf());
        let target = super::super::bash::SpillTarget::from_context(&ctx);
        let out = spill_over_budget(ToolOutput::text(full), target.as_ref()).await;

        assert!(
            out.content.trim_end().ends_with("…(truncated)"),
            "ended on {:?}",
            out.content.lines().next_back()
        );
    }

    /// No file, no promises: a spill that could not be written must not
    /// leave the result claiming one holds the rest.
    #[tokio::test]
    async fn an_unwritable_spill_does_not_claim_the_file_has_the_matches() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, b"").unwrap();
        let ctx = ToolContext::root(std::path::PathBuf::from("."))
            .with_spill_dir(blocker.join("tool-output"));
        let target = super::super::bash::SpillTarget::from_context(&ctx);
        let full = "src/a.rs:1:x\n".repeat(super::super::bash::MAX_PREVIEW / 12 + 200);

        let out = spill_over_budget(ToolOutput::text(full), target.as_ref()).await;

        assert!(
            out.content.starts_with("(could not save the full output:"),
            "{}",
            out.content.lines().next().unwrap_or_default()
        );
        assert!(
            !out.content.contains("every match"),
            "promised a file that is not there"
        );
        assert!(out.content.contains("src/a.rs:1:x"), "the preview was lost");
    }

    /// The match cap only short-circuits when matches exist; a rare or
    /// no-match search over a monorepo needs its own bound, and it must
    /// be distinguishable from "capped at 200 matches".
    #[test]
    fn entry_budget_truncation_is_distinct_from_the_match_cap() {
        let dir = tempfile::tempdir().unwrap();
        for index in 0..20 {
            std::fs::write(
                dir.path().join(format!("f{index}.txt")),
                "no needles here\n",
            )
            .unwrap();
        }
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let out = grep_files(
            dir.path(),
            dir.path(),
            &plain("zzz-absent"),
            false,
            MAX_MATCHES,
            5,
            &cancelled,
        );
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("scanned 5 files"),
            "expected an entry-budget notice: {}",
            out.content
        );
        assert!(
            !out.content.contains("truncated)"),
            "must not read as a match-cap truncation: {}",
            out.content
        );
    }
}
