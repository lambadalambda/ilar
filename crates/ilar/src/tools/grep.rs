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

pub struct GrepTool;

#[derive(Deserialize)]
struct Input {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    /// Search gitignored files too. Off by default.
    #[serde(default)]
    include_ignored: bool,
}

/// One rendered `path:line:text` hit, kept with its sort key so parallel
/// walking cannot reorder the output.
struct Hit {
    path: String,
    line: usize,
    rendered: String,
}

impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> &'static str {
        "Search file contents with a regex, recursively from cwd (or path, \
         which may be relative to cwd or absolute). \
         Gitignored files are skipped unless include_ignored is set. \
         Returns file:line:match."
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
            let spill = super::bash::SpillTarget::from_context(&ctx);
            match super::blocking_scan(move |cancelled| {
                grep_files(
                    &ctx.cwd,
                    &root,
                    &input.pattern,
                    input.include_ignored,
                    MAX_ENTRIES,
                    &cancelled,
                )
            })
            .await
            {
                Ok(output) => spill_over_budget(output, spill.as_ref()).await,
                Err(error) => ToolOutput::error(format!("grep worker failed: {error}")),
            }
        })
    }
}

/// Scan one file. Pure apart from reading it: returns the hits and
/// whether the file's byte cap clipped the scan.
fn grep_one_file(
    path: &std::path::Path,
    relative: &str,
    regex: &regex::Regex,
    cancelled: &std::sync::atomic::AtomicBool,
) -> (Vec<Hit>, bool) {
    let Ok(file) = std::fs::File::open(path) else {
        return (Vec::new(), false);
    };
    let mut reader = std::io::BufReader::new(file).take(MAX_FILE_BYTES + 1);
    let mut hits = Vec::new();
    let mut truncated = false;
    let mut line = Vec::new();
    let mut line_number = 0_usize;
    let mut file_bytes = 0_u64;
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
            truncated = true;
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
        if regex.is_match(&text) {
            let mut rendered = format!("{relative}:{line_number}:{}", text.trim_end());
            truncate_bytes_ellipsis(&mut rendered, MAX_OUTPUT_LINE_BYTES);
            hits.push(Hit {
                path: relative.to_string(),
                line: line_number,
                rendered,
            });
            if hits.len() >= MAX_MATCHES_PER_FILE {
                truncated = true;
                break;
            }
        }
        if file_limit_reached {
            break;
        }
    }
    (hits, truncated)
}

fn grep_files(
    cwd: &std::path::Path,
    root: &std::path::Path,
    pattern: &str,
    include_ignored: bool,
    max_entries: usize,
    cancelled: &std::sync::atomic::AtomicBool,
) -> ToolOutput {
    let regex = match regex::Regex::new(pattern) {
        Ok(regex) => regex,
        Err(error) => return ToolOutput::error(format!("grep: invalid regex: {error}")),
    };
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
    let scanned = std::sync::atomic::AtomicUsize::new(0);
    let clipped = std::sync::atomic::AtomicBool::new(false);
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
            if hits.lock().unwrap().len() >= MAX_MATCHES {
                return ignore::WalkState::Quit;
            }
            let relative = entry
                .path()
                .strip_prefix(cwd)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .into_owned();
            let (found, file_clipped) = grep_one_file(entry.path(), &relative, &regex, cancelled);
            if file_clipped {
                clipped.store(true, Ordering::Release);
            }
            if !found.is_empty() {
                hits.lock().unwrap().extend(found);
            }
            ignore::WalkState::Continue
        })
    });

    if cancelled.load(Ordering::Acquire) {
        return ToolOutput::error("cancelled");
    }
    let mut hits = hits.into_inner().unwrap();
    // Parallel walking loses walk order; make the output reproducible.
    hits.sort_by(|left, right| left.path.cmp(&right.path).then(left.line.cmp(&right.line)));
    let mut truncated = clipped.load(Ordering::Acquire);
    if hits.len() > MAX_MATCHES {
        hits.truncate(MAX_MATCHES);
        truncated = true;
    }
    let mut out = String::new();
    for hit in hits {
        if out.len().saturating_add(hit.rendered.len() + 1) > MAX_OUTPUT_BYTES {
            truncated = true;
            break;
        }
        out.push_str(&hit.rendered);
        out.push('\n');
    }
    if capped_entries.load(Ordering::Acquire) {
        close_with(
            &mut out,
            &format!(
                "…(truncated: scanned {max_entries} files without finishing; narrow the path)"
            ),
            MAX_OUTPUT_BYTES,
        );
    } else if truncated {
        close_with(&mut out, "…(truncated)", MAX_OUTPUT_BYTES);
    }
    ToolOutput::text(out)
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
            "needle",
            false,
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
            out.content.trim_end().ends_with("…(truncated)"),
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
        let out = grep_files(dir.path(), dir.path(), "zzz-absent", false, 5, &cancelled);
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
