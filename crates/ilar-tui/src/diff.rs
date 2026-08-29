//! Dependency-free line diff for edit-tool transcript rendering — see
//! meta/issues/render-edit-diffs.md.

/// Per-side line cap; beyond this the quadratic LCS is skipped entirely.
const MAX_DIFF_LINES: usize = 400;
/// Total input cap so huge single-line payloads are never stored and
/// re-wrapped by the renderer every frame.
const MAX_DIFF_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    Added,
    Removed,
    Context,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffKind,
    pub text: String,
}

fn line(kind: DiffKind, text: &str) -> DiffLine {
    DiffLine {
        kind,
        text: text.to_string(),
    }
}

/// LCS line diff. Returns `None` when either side exceeds the size cap so
/// callers can fall back to plain-text rendering.
pub fn diff_lines(old: &str, new: &str) -> Option<Vec<DiffLine>> {
    if old.len().saturating_add(new.len()) > MAX_DIFF_BYTES {
        return None;
    }
    let old: Vec<&str> = old.lines().collect();
    let new: Vec<&str> = new.lines().collect();
    if old.len() > MAX_DIFF_LINES || new.len() > MAX_DIFF_LINES {
        return None;
    }
    // Lengths of the longest common subsequence of old[i..] and new[j..].
    let mut lcs = vec![vec![0u16; new.len() + 1]; old.len() + 1];
    for i in (0..old.len()).rev() {
        for j in (0..new.len()).rev() {
            lcs[i][j] = if old[i] == new[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut output = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < old.len() && j < new.len() {
        if old[i] == new[j] {
            output.push(line(DiffKind::Context, old[i]));
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            output.push(line(DiffKind::Removed, old[i]));
            i += 1;
        } else {
            output.push(line(DiffKind::Added, new[j]));
            j += 1;
        }
    }
    output.extend(old[i..].iter().map(|text| line(DiffKind::Removed, text)));
    output.extend(new[j..].iter().map(|text| line(DiffKind::Added, text)));
    // A diff without ± lines (e.g. a trailing-newline-only change, which
    // `str::lines` cannot see) would silently suppress the caller's
    // plain-text fallback while showing nothing changed.
    if output.iter().all(|line| line.kind == DiffKind::Context) {
        return None;
    }
    Some(output)
}

/// Diff for a tool call's raw arguments JSON. Empty for tools that do
/// not change files, unparseable arguments, or strings too large to
/// diff — callers fall back to plain-text argument rendering.
pub fn tool_diff(name: &str, arguments: &str) -> Vec<DiffLine> {
    // Duplicates tool_diff_value's dispatch on purpose: it skips the
    // JSON parse entirely for the overwhelmingly common other tools.
    if !matches!(name, "edit" | "write") {
        return Vec::new();
    }
    serde_json::from_str(arguments)
        .ok()
        .map(|value| tool_diff_value(name, &value))
        .unwrap_or_default()
}

/// [`tool_diff`] for already-parsed input (session restore path).
pub fn tool_diff_value(name: &str, input: &serde_json::Value) -> Vec<DiffLine> {
    match name {
        "edit" => edit_diff(input),
        "write" => write_diff(input),
        _ => Vec::new(),
    }
}

fn edit_diff(input: &serde_json::Value) -> Vec<DiffLine> {
    let old = input.get("old_string").and_then(serde_json::Value::as_str);
    let new = input.get("new_string").and_then(serde_json::Value::as_str);
    match (old, new) {
        (Some(old), Some(new)) => diff_lines(old, new).unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// A `write` is a file change too (the web already draws it as one —
/// the TUI showing the body as escaped JSON was the drift): a new file
/// is its content as pure additions, and when the arguments carry the
/// previous content (`old_content`/`old_string`; the stock write tool
/// sends only `path` + `content`) it is a real diff.
///
/// Pure additions have no LCS to bound, so only the byte cap applies —
/// [`MAX_DIFF_LINES`] exists to bound a quadratic match that never runs
/// here, and must not send a 500-line new file back to truncated JSON.
fn write_diff(input: &serde_json::Value) -> Vec<DiffLine> {
    let Some(new) = input.get("content").and_then(serde_json::Value::as_str) else {
        return Vec::new();
    };
    if new.len() > MAX_DIFF_BYTES {
        return Vec::new();
    }
    let old = ["old_content", "old_string"]
        .iter()
        .find_map(|key| input.get(key).and_then(serde_json::Value::as_str));
    match old {
        // An overwrite whose diff falls past the LCS caps (or that the
        // differ sees as changeless) still puts this whole body on
        // disk: show it like a new file rather than falling back to
        // escaped JSON.
        Some(old) if !old.is_empty() => diff_lines(old, new).unwrap_or_else(|| written_lines(new)),
        _ => written_lines(new),
    }
}

/// A written body, every line of it an addition.
fn written_lines(content: &str) -> Vec<DiffLine> {
    content
        .lines()
        .map(|text| line(DiffKind::Added, text))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(diff: &[DiffLine]) -> Vec<DiffKind> {
        diff.iter().map(|line| line.kind).collect()
    }

    #[test]
    fn single_line_change_keeps_surrounding_context() {
        let diff = diff_lines("a\nb\nc", "a\nB\nc").unwrap();
        assert_eq!(
            kinds(&diff),
            vec![
                DiffKind::Context,
                DiffKind::Removed,
                DiffKind::Added,
                DiffKind::Context
            ]
        );
        assert_eq!(diff[1].text, "b");
        assert_eq!(diff[2].text, "B");
    }

    #[test]
    fn pure_insertion_and_deletion() {
        let insertion = diff_lines("a\nc", "a\nb\nc").unwrap();
        assert_eq!(
            kinds(&insertion),
            vec![DiffKind::Context, DiffKind::Added, DiffKind::Context]
        );
        let deletion = diff_lines("a\nb\nc", "a\nc").unwrap();
        assert_eq!(
            kinds(&deletion),
            vec![DiffKind::Context, DiffKind::Removed, DiffKind::Context]
        );
    }

    #[test]
    fn disjoint_replacement_lists_removals_before_additions_at_the_tail() {
        let diff = diff_lines("old only", "new only").unwrap();
        assert_eq!(kinds(&diff), vec![DiffKind::Removed, DiffKind::Added]);
    }

    #[test]
    fn multibyte_lines_are_compared_whole() {
        let diff = diff_lines("İstanbul\nx", "İstanbul\ny").unwrap();
        assert_eq!(diff[0].kind, DiffKind::Context);
        assert_eq!(diff[0].text, "İstanbul");
    }

    #[test]
    fn oversized_inputs_fall_back() {
        let big = "line\n".repeat(MAX_DIFF_LINES + 1);
        assert!(diff_lines(&big, "x").is_none());
        assert!(diff_lines("x", &big).is_none());
        let huge_single_line = "x".repeat(MAX_DIFF_BYTES + 1);
        assert!(diff_lines(&huge_single_line, "x").is_none());
    }

    #[test]
    fn changeless_diffs_fall_back_to_plain_rendering() {
        assert!(diff_lines("same", "same").is_none());
        assert!(diff_lines("", "").is_none());
        // str::lines cannot represent a trailing-newline-only change.
        assert!(diff_lines("foo", "foo\n").is_none());
    }

    #[test]
    fn empty_sides_produce_pure_add_or_remove() {
        let all_added = diff_lines("", "a\nb").unwrap();
        assert_eq!(kinds(&all_added), vec![DiffKind::Added, DiffKind::Added]);
        let all_removed = diff_lines("a\nb", "").unwrap();
        assert_eq!(
            kinds(&all_removed),
            vec![DiffKind::Removed, DiffKind::Removed]
        );
    }

    #[test]
    fn tool_diff_parses_edit_arguments() {
        let arguments =
            serde_json::json!({"path": "f.rs", "old_string": "a\nb", "new_string": "a\nc"})
                .to_string();
        let diff = tool_diff("edit", &arguments);
        assert_eq!(
            kinds(&diff),
            vec![DiffKind::Context, DiffKind::Removed, DiffKind::Added]
        );
    }

    #[test]
    fn tool_diff_is_empty_for_other_tools_or_malformed_arguments() {
        let edit_arguments = serde_json::json!({"old_string": "a", "new_string": "b"}).to_string();
        assert!(tool_diff("read", &edit_arguments).is_empty());
        assert!(tool_diff("edit", "not json").is_empty());
        assert!(tool_diff("write", "not json").is_empty());
        assert!(tool_diff("edit", r#"{"path": "f.rs"}"#).is_empty());
        assert!(tool_diff("edit", r#"{"old_string": 3, "new_string": "x"}"#).is_empty());
        // Edit-shaped arguments under a write name have no `content` to
        // show, so there is nothing to draw.
        assert!(tool_diff("write", &edit_arguments).is_empty());
    }

    #[test]
    fn a_write_is_its_body_as_pure_additions() {
        let arguments = serde_json::json!({"path": "f.rs", "content": "a\nb"}).to_string();
        let diff = tool_diff("write", &arguments);
        assert_eq!(kinds(&diff), vec![DiffKind::Added, DiffKind::Added]);
        assert_eq!(diff[0].text, "a");
        assert_eq!(diff[1].text, "b");
        // A new file is pure addition: the LCS line cap bounds a match
        // that never runs here, so a long file still gets its diff.
        let long = "line\n".repeat(MAX_DIFF_LINES + 100);
        let diff = tool_diff("write", &serde_json::json!({"content": long}).to_string());
        assert_eq!(diff.len(), MAX_DIFF_LINES + 100);
        assert!(diff.iter().all(|line| line.kind == DiffKind::Added));
    }

    #[test]
    fn a_write_with_old_content_is_a_real_diff() {
        let arguments =
            serde_json::json!({"path": "f.rs", "old_content": "a\nb", "content": "a\nc"})
                .to_string();
        assert_eq!(
            kinds(&tool_diff("write", &arguments)),
            vec![DiffKind::Context, DiffKind::Removed, DiffKind::Added]
        );
        // Past the LCS caps (or when the differ sees no change) the
        // overwrite still shows the body it wrote, as additions.
        let big_old = "x\n".repeat(MAX_DIFF_LINES + 1);
        let arguments = serde_json::json!({"old_content": big_old, "content": "a\nb"}).to_string();
        assert_eq!(
            kinds(&tool_diff("write", &arguments)),
            vec![DiffKind::Added, DiffKind::Added]
        );
        let arguments = serde_json::json!({"old_content": "a\nb", "content": "a\nb"}).to_string();
        assert_eq!(
            kinds(&tool_diff("write", &arguments)),
            vec![DiffKind::Added, DiffKind::Added]
        );
    }

    #[test]
    fn write_diffs_fall_back_past_the_byte_cap_or_without_content() {
        let enormous = "x".repeat(MAX_DIFF_BYTES + 1);
        assert!(
            tool_diff(
                "write",
                &serde_json::json!({"content": enormous}).to_string()
            )
            .is_empty()
        );
        assert!(tool_diff("write", &serde_json::json!({"path": "f.rs"}).to_string()).is_empty());
        // An empty body has no lines: plain rendering says "wrote
        // nothing" better than an empty diff block.
        assert!(tool_diff("write", &serde_json::json!({"content": ""}).to_string()).is_empty());
    }
}
