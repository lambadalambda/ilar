//! edit: exact-match string replacement, gated on what the model has
//! actually seen. Errors on zero or multiple matches unless replace_all;
//! a no-match error carries the closest region of the file, so the model
//! can correct in one round trip instead of guessing again.

use serde::Deserialize;

use super::{
    SeenFiles, Tool, ToolConcurrency, ToolContext, ToolFuture, ToolOutput, WorkspaceAccess,
    WorkspaceCoverage, parse_input, run_blocking_io,
};

/// Edits load the whole file plus a rewritten copy, so the resident cost
/// is a few times this. Generous enough for lockfiles and fixtures. The
/// tracking cap is the same number: a file too big to edit is not worth
/// hashing.
const MAX_FILE_BYTES: u64 = super::MAX_TRACKED_FILE_BYTES;

/// Bounds on the no-match diagnostic. It runs on an error path, on a file
/// the model just failed to match, so it must stay cheap enough to be
/// invisible: at most this many window starts, each scored on at most
/// [`MAX_PROBE_LINES`] lines of `old_string`, each capped at
/// [`MAX_LINE_BYTES`] bytes.
const MAX_SCAN_LINES: usize = 5_000;
const MAX_PROBE_LINES: usize = 20;
const MAX_LINE_BYTES: usize = 400;

/// Below this average line similarity the closest window is not close
/// enough to be worth quoting — saying "nothing matches" is the more
/// honest answer.
const MIN_REGION_SCORE: f64 = 0.4;

/// How much of a quoted line the error carries.
const MAX_REPORTED_LINE_BYTES: usize = 200;

const UNREAD: &str = "you have not read this file in this session; read it first";
const CHANGED: &str = "the file changed since you last read it (a command or another process \
                       wrote it); re-read before editing";

pub struct EditTool;

#[derive(Deserialize)]
struct Input {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn description(&self) -> &'static str {
        "Replace text in a file. Read the file first: edit refuses a file \
         this session has not read, and refuses again if the file changed \
         since that read (re-read it then). old_string must match the \
         current contents exactly and match once unless replace_all is \
         true — include surrounding lines to disambiguate, and never \
         include read's \"N→\" line-number prefixes."
    }

    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Barrier
    }
    fn workspace_access(&self) -> WorkspaceAccess {
        WorkspaceAccess::Mutating
    }

    /// Like write: take the executor's lease and hold it inside the
    /// blocking task, so a dropped future cannot release it while the
    /// file is being written. Both flags must agree — leaving
    /// `manages_workspace_access` false routes the executor down the
    /// permit branch, and acquiring a lease on top of that permit
    /// deadlocks on the same workspace lock.
    fn manages_workspace_access(&self) -> bool {
        true
    }

    fn accepts_executor_workspace_lease(&self) -> bool {
        true
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "old_string": {"type": "string"},
                "new_string": {"type": "string"},
                "replace_all": {"type": "boolean"}
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    fn run(&self, input: serde_json::Value, ctx: ToolContext) -> ToolFuture {
        Box::pin(async move {
            let input: Input = match parse_input(input, "edit") {
                Ok(v) => v,
                Err(e) => return e,
            };
            if input.old_string == input.new_string {
                return ToolOutput::error("old_string and new_string are identical");
            }
            let lease = match ctx.workspace_coverage(WorkspaceAccess::Mutating) {
                WorkspaceCoverage::Covered => ctx
                    .workspace_lease
                    .expect("covered workspace access has a lease"),
                WorkspaceCoverage::Absent => {
                    ctx.workspace.acquire_lease(WorkspaceAccess::Mutating).await
                }
                WorkspaceCoverage::Incompatible => {
                    return ToolOutput::error(
                        "edit requests workspace access not covered by its inherited lease",
                    );
                }
            };
            let cancel = ctx.cancel;
            let path = ctx.cwd.join(&input.path);
            let display_path = input.path.clone();
            let seen_files = ctx.seen_files.clone();
            let result = run_blocking_io(lease, move || {
                replace_in_file(&path, &input, &seen_files, &cancel)
            })
            .await;

            match result {
                Ok(replacements) => ToolOutput::text(format!(
                    "edited {display_path}: {replacements} replacement{}",
                    if replacements > 1 { "s" } else { "" }
                )),
                Err(error) => ToolOutput::error(format!("edit {display_path}: {error}")),
            }
        })
    }
}

fn interrupted(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Interrupted, message)
}

/// Read, replace, and atomically write. Returns the replacement count.
fn replace_in_file(
    path: &std::path::Path,
    input: &Input,
    seen_files: &SeenFiles,
    cancel: &tokio_util::sync::CancellationToken,
) -> std::io::Result<usize> {
    if cancel.is_cancelled() {
        return Err(interrupted("edit cancelled"));
    }
    let size = std::fs::metadata(path)?.len();
    if size > MAX_FILE_BYTES {
        return Err(std::io::Error::other(format!(
            "file is too large to edit ({size} bytes, cap {MAX_FILE_BYTES}); \
             narrow the change or rewrite it with write"
        )));
    }
    let content = std::fs::read_to_string(path)?;
    // The gate: an edit is a claim about text the model believes is in
    // this file, and the claim is only worth anything if the model has
    // seen this version of it. Weak models edit from a stale mental copy
    // — including after rewriting the file through bash — and then loop
    // on "old_string not found".
    match seen_files.digest_of(path) {
        None => return Err(std::io::Error::other(UNREAD)),
        Some(seen) if seen != super::digest(content.as_bytes()) => {
            return Err(std::io::Error::other(CHANGED));
        }
        Some(_) => {}
    }
    let matches = content.matches(&input.old_string).count();
    // Checked before anything is written, and only when something would
    // be: a no-match edit has its own, more informative diagnostics
    // below, but a *matching* one is about to put read's line numbers in
    // the file for real.
    if matches > 0
        && let Some(pasted) = pasted_read_output(input)
    {
        return Err(std::io::Error::other(pasted));
    }
    let (new_content, replacements) = match (matches, input.replace_all) {
        (0, _) => return Err(std::io::Error::other(no_match_error(&content, input))),
        (1, _) => (content.replacen(&input.old_string, &input.new_string, 1), 1),
        (n, true) => (content.replace(&input.old_string, &input.new_string), n),
        (n, false) => {
            return Err(std::io::Error::other(format!(
                "old_string matches {n} times; add surrounding context to make it unique, \
                 or set replace_all"
            )));
        }
    };
    // Last check before the replace commits; the atomic write is
    // all-or-nothing, so an abort here leaves the original intact.
    if cancel.is_cancelled() {
        return Err(interrupted("edit cancelled"));
    }
    crate::atomic_file::replace_cancellable(
        path,
        new_content.as_bytes(),
        crate::atomic_file::Mode::Preserve,
        cancel,
    )?;
    // The model knows what it just wrote, so the next edit needs no
    // re-read.
    seen_files.record(path, new_content.as_bytes());
    Ok(replacements)
}

/// The whole of what a model gets told when its `old_string` matched
/// nothing: first the mechanical mistake, if it made one, then the
/// closest thing in the file to what it asked for.
fn no_match_error(content: &str, input: &Input) -> String {
    if let Some(contamination) = line_number_contamination(input) {
        return contamination;
    }
    match nearest_region(content, &input.old_string) {
        Some(region) => format!(
            "old_string not found. The closest text in the file is {}:\n{}\n\
             copy it into old_string exactly as it stands there (without the \
             \"N→\" line numbers), or re-read the file",
            region.location(),
            region.quoted()
        ),
        None => "old_string not found, and nothing in the file is close to it; \
                 re-read the file to see what it says now"
            .into(),
    }
}

/// Read numbers its output `N→line`. A model that copies that back into
/// `old_string` can never match, and the bare "not found" tells it
/// nothing — so the mistake gets named, with the field that made it.
fn line_number_contamination(input: &Input) -> Option<String> {
    let fields: Vec<&str> = [
        ("old_string", &input.old_string),
        ("new_string", &input.new_string),
    ]
    .into_iter()
    .filter(|(_, text)| text.lines().any(has_line_number_prefix))
    .map(|(field, _)| field)
    .collect();
    let subject = match fields.as_slice() {
        [] => return None,
        [field] => format!("{field} still carries"),
        _ => format!("{} still carry", fields.join(" and ")),
    };
    Some(format!(
        "{subject} read's line numbers (lines like \"12→…\"); \
         copy the file's own text, without the \"N→\" prefix"
    ))
}

/// The other half of the same mistake, and the destructive one: this
/// `old_string` matched, so the replacement is about to be written, and
/// `new_string` is a chunk of read's output — the prefixes would land in
/// the file.
///
/// Asymmetric on purpose. A file that genuinely contains `N→` text is
/// one whose `old_string` was copied out of it and carries the prefixes
/// too, so contamination on *both* sides means the file really does look
/// like that and the edit is legitimate. A clean `old_string` with a
/// prefixed `new_string` is the model pasting read's output as its
/// replacement. Two consecutive, increasing numbers are the signature:
/// one `12→…` line is something a document may legitimately quote.
fn pasted_read_output(input: &Input) -> Option<String> {
    if input.old_string.lines().any(has_line_number_prefix) {
        return None;
    }
    let numbered: Vec<Option<u64>> = input.new_string.lines().map(line_number_prefix).collect();
    let pasted = numbered.windows(2).any(|pair| match pair {
        [Some(first), Some(second)] => *second == first.saturating_add(1),
        _ => false,
    });
    pasted.then(|| {
        "new_string carries read's line numbers (lines like \"12→…\") — the file would end up \
         containing them; copy the replacement text without the prefixes (or use write for \
         content that really should look like that)"
            .to_string()
    })
}

fn has_line_number_prefix(line: &str) -> bool {
    line_number_prefix(line).is_some()
}

/// The `N` of read's `N→` prefix, when the line carries one.
fn line_number_prefix(line: &str) -> Option<u64> {
    let rest = line.trim_start();
    let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 || !rest[digits..].starts_with('→') {
        return None;
    }
    rest[..digits].parse().ok()
}

/// A run of file lines quoted back to the model, 1-based.
#[derive(Debug, PartialEq)]
struct Region {
    start: usize,
    lines: Vec<String>,
}

impl Region {
    fn location(&self) -> String {
        match self.lines.len() {
            1 => format!("line {}", self.start),
            n => format!("lines {}-{}", self.start, self.start + n - 1),
        }
    }

    /// Read's own presentation, so the model sees the region the way it
    /// saw the file. Bounded by construction in lines (a region is at
    /// most [`MAX_PROBE_LINES`] long), so only line length is capped
    /// here.
    fn quoted(&self) -> String {
        let mut out = String::new();
        for (offset, line) in self.lines.iter().enumerate() {
            let mut text = line.clone();
            crate::text::truncate_bytes(&mut text, MAX_REPORTED_LINE_BYTES);
            if text.len() < line.len() {
                text.push('…');
            }
            out.push_str(&format!("{}→{text}\n", self.start + offset));
        }
        out
    }
}

/// The window of `content` that looks most like `old_string`, compared
/// with whitespace collapsed — the drift is usually indentation, a
/// renamed identifier or a line the model half-remembers, none of which
/// an exact match can see. `None` when nothing scores above
/// [`MIN_REGION_SCORE`], or when the file is too big to scan within the
/// bounds this diagnostic is allowed.
fn nearest_region(content: &str, old_string: &str) -> Option<Region> {
    let probe: Vec<String> = old_string
        .lines()
        .take(MAX_PROBE_LINES)
        .map(normalize)
        .collect();
    if probe.is_empty() {
        return None;
    }
    // `lines()` is lazy and this is where it stops: the tail of a huge
    // file is never walked, let alone normalized. Everything downstream
    // is bounded by this slice.
    let file: Vec<&str> = content.lines().take(MAX_SCAN_LINES + probe.len()).collect();
    let window = probe.len().min(file.len());
    if window == 0 {
        return None;
    }
    let normalized: Vec<String> = file.iter().copied().map(normalize).collect();
    let (start, score) = (0..=file.len() - window)
        .map(|start| (start, window_score(&normalized[start..], &probe)))
        .max_by(|(_, a), (_, b)| a.total_cmp(b))?;
    (score >= MIN_REGION_SCORE).then(|| Region {
        start: start + 1,
        lines: file[start..start + window]
            .iter()
            .map(|line| (*line).to_string())
            .collect(),
    })
}

fn window_score(window: &[String], probe: &[String]) -> f64 {
    let scored = probe.len().min(window.len());
    if scored == 0 {
        return 0.0;
    }
    let total: f64 = (0..scored)
        .map(|index| line_score(&window[index], &probe[index]))
        .sum();
    // Missing lines (a window running off the end of the file) score
    // zero rather than being ignored.
    total / probe.len() as f64
}

/// 1.0 for lines equal once whitespace is collapsed, otherwise a cheap
/// shared prefix/suffix ratio — enough to tell "the same line with one
/// identifier changed" from "an unrelated line", without the cost of a
/// real edit distance on every window of the file.
fn line_score(a: &str, b: &str) -> f64 {
    if a == b {
        return 1.0;
    }
    let a = &a.as_bytes()[..a.len().min(MAX_LINE_BYTES)];
    let b = &b.as_bytes()[..b.len().min(MAX_LINE_BYTES)];
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let remaining = a.len().min(b.len()) - prefix;
    let suffix = a
        .iter()
        .rev()
        .zip(b.iter().rev())
        .take_while(|(x, y)| x == y)
        .count()
        .min(remaining);
    2.0 * (prefix + suffix) as f64 / (a.len() + b.len()) as f64
}

fn normalize(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(old_string: &str, new_string: &str) -> Input {
        Input {
            path: "a.txt".into(),
            old_string: old_string.into(),
            new_string: new_string.into(),
            replace_all: false,
        }
    }

    #[test]
    fn a_line_number_prefix_is_digits_then_an_arrow() {
        assert!(has_line_number_prefix("12→let x = 1;"));
        assert!(has_line_number_prefix("    7→    indented"));
        assert!(!has_line_number_prefix("12 → spaced"));
        assert!(!has_line_number_prefix("→no number"));
        assert!(!has_line_number_prefix("let arrow = \"a→b\";"));
        assert!(!has_line_number_prefix(""));
    }

    #[test]
    fn contamination_names_the_field_that_carries_the_line_numbers() {
        let message = line_number_contamination(&input("1→alpha", "alpha")).unwrap();
        assert!(message.starts_with("old_string still carries"), "{message}");
        assert!(!message.contains("new_string"), "{message}");

        let message = line_number_contamination(&input("alpha", "1→alpha")).unwrap();
        assert!(message.starts_with("new_string still carries"), "{message}");
        assert!(!message.contains("old_string"), "{message}");

        let message = line_number_contamination(&input("1→alpha", "1→beta")).unwrap();
        assert!(
            message.starts_with("old_string and new_string still carry"),
            "{message}"
        );

        assert_eq!(line_number_contamination(&input("alpha", "beta")), None);
    }

    #[test]
    fn a_line_number_prefix_yields_its_number() {
        assert_eq!(line_number_prefix("12→let x = 1;"), Some(12));
        assert_eq!(line_number_prefix("    7→    indented"), Some(7));
        assert_eq!(line_number_prefix("0→first"), Some(0));
        assert_eq!(line_number_prefix("12 → spaced"), None);
    }

    /// Only the shape of pasted read output fires it: two consecutive,
    /// increasing numbers, and only when old_string is clean.
    #[test]
    fn a_pasted_read_output_replacement_is_recognised_by_its_numbering() {
        assert!(pasted_read_output(&input("alpha\nbeta", "1→alpha\n2→BETA")).is_some());
        assert!(pasted_read_output(&input("alpha", "  11→alpha\n  12→BETA")).is_some());

        // One quoted line is not a paste.
        assert_eq!(pasted_read_output(&input("alpha", "12→alpha")), None);
        // Numbers that are neither consecutive nor increasing are not
        // read's output either.
        assert_eq!(pasted_read_output(&input("alpha", "1→a\n7→b")), None);
        assert_eq!(pasted_read_output(&input("alpha", "2→a\n1→b")), None);
        assert_eq!(pasted_read_output(&input("alpha", "1→a\nplain\n2→b")), None);
        // Symmetric contamination: the file really looks like that.
        assert_eq!(
            pasted_read_output(&input("1→alpha\n2→beta", "1→alpha\n2→BETA")),
            None
        );
        assert_eq!(pasted_read_output(&input("alpha", "beta")), None);
    }

    /// The mechanical mistake is named before the model is invited to
    /// compare its text against the file.
    #[test]
    fn contamination_pre_empts_the_nearest_match_report() {
        let error = no_match_error("alpha\nbeta\n", &input("1→alpha", "1→ALPHA"));
        assert!(error.contains("still carry read's line numbers"), "{error}");
        assert!(!error.contains("closest"), "{error}");
    }

    #[test]
    fn the_nearest_region_is_the_drifted_original_with_its_line_number() {
        let content = "fn main() {\n    let total = compute(2);\n    print(total);\n}\n";
        let region = nearest_region(content, "    let total = compute(1);").unwrap();

        assert_eq!(
            region,
            Region {
                start: 2,
                lines: vec!["    let total = compute(2);".into()],
            }
        );
        assert_eq!(region.location(), "line 2");
        assert_eq!(region.quoted(), "2→    let total = compute(2);\n");
    }

    /// Indentation drift is the common case, and collapsing whitespace is
    /// how the *diagnostic* sees past it — the match itself stays exact.
    #[test]
    fn a_multi_line_region_survives_reindentation() {
        let content =
            "struct S;\n\nimpl S {\n        fn go(&self) {\n            work();\n        }\n}\n";
        let region = nearest_region(content, "fn go(&self) {\n    work();\n}").unwrap();

        assert_eq!(region.start, 4);
        assert_eq!(region.lines.len(), 3);
        assert_eq!(region.location(), "lines 4-6");
    }

    #[test]
    fn nothing_close_is_reported_as_nothing_close() {
        assert_eq!(
            nearest_region("alpha\nbeta\ngamma\n", "zzzzzzzzzzzzzzzzzzzzzzzz"),
            None
        );
        assert_eq!(nearest_region("", "alpha"), None);
        assert_eq!(nearest_region("alpha\n", ""), None);

        let error = no_match_error("alpha\n", &input("zzzzzzzzzzzzzzzzzzzzzzzz", "y"));
        assert!(
            error.contains("nothing in the file is close to it"),
            "{error}"
        );
    }

    /// The scan is capped, so a diagnostic on a huge file cannot cost
    /// more than the edit it is explaining. The needle here sits past the
    /// cap and is therefore not found — that is the price of the bound.
    #[test]
    fn the_scan_stops_at_its_line_cap() {
        let mut content = "filler\n".repeat(MAX_SCAN_LINES + 10);
        content.push_str("let total = compute(2);\n");
        assert_eq!(nearest_region(&content, "let total = compute(1);"), None);
        assert!(nearest_region(&content[..1000], "filler").is_some());
    }

    /// However long `old_string` is, the report is a bounded number of
    /// bounded lines — an error the model has to read is no place for
    /// half a file.
    #[test]
    fn a_quoted_region_is_bounded_in_lines_and_line_length() {
        let long_line = format!("let x = {};\n", "y".repeat(MAX_REPORTED_LINE_BYTES * 2));
        let content = long_line.repeat(MAX_PROBE_LINES * 3);
        let region = nearest_region(&content, &long_line.repeat(MAX_PROBE_LINES * 2)).unwrap();
        let quoted = region.quoted();

        assert_eq!(region.lines.len(), MAX_PROBE_LINES);
        assert_eq!(quoted.lines().count(), MAX_PROBE_LINES);
        for line in quoted.lines() {
            assert!(line.len() <= MAX_REPORTED_LINE_BYTES + 16, "{line}");
            assert!(line.ends_with('…'), "{line}");
        }
    }
}
