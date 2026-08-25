//! Bounded truncation of text, in one place.
//!
//! Three axes, deliberately kept apart:
//!
//! - **bytes** — a wire/output budget ([`truncate_bytes`],
//!   [`truncate_bytes_ellipsis`], [`tail_bytes`], [`tail_str`]);
//! - **chars** — a codepoint budget for titles and previews
//!   ([`truncate_chars`], [`truncate_chars_ellipsis`]);
//! - **display width** — columns on a terminal, which is
//!   grapheme-and-width business and lives in `ilar-tui`'s own `text`
//!   module, not here.
//!
//! The ellipsis variants differ on purpose, matching what their callers
//! already promised: the byte variant fits `…` *inside* the cap (an
//! output budget is a hard limit), the char variant appends it *on top*
//! (a title of N chars plus a hint that it was cut).

const ELLIPSIS: char = '…';

/// Byte counts the way every surface writes them: the live tool row, the
/// restored row, image markers and the attachment notices.
pub fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// How much of a tool result any surface keeps.
pub const MAX_DETAIL_CHARS: usize = 16 * 1024;

/// Marker appended when [`truncate_detail`] cut something.
pub const DETAIL_TRUNCATED: &str = "\n… output truncated";

/// Cap a tool detail at [`MAX_DETAIL_CHARS`]. The live row bounds the
/// text as it streams and the restored row bounds it again on reload;
/// both call this, so the two halves of a description cannot be cut at
/// different lengths or marked with different words.
pub fn truncate_detail(mut detail: String) -> String {
    if detail.chars().count() > MAX_DETAIL_CHARS {
        detail = detail.chars().take(MAX_DETAIL_CHARS).collect();
        detail.push_str(DETAIL_TRUNCATED);
    }
    detail
}

/// The largest char-boundary index at or below `end`.
///
/// This is the only UTF-8 boundary walk in the crate; everything that
/// cuts a string goes through here or through [`tail_start`].
fn floor_boundary(value: &str, mut end: usize) -> usize {
    if end >= value.len() {
        return value.len();
    }
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// Where the last `keep` bytes start, moved forward off any UTF-8
/// continuation byte so a tail cut mid-codepoint does not open with a
/// replacement character. Works on possibly-invalid bytes too, which is
/// why it tests the continuation bits rather than `is_char_boundary`.
fn tail_start(bytes: &[u8], keep: usize) -> usize {
    if keep >= bytes.len() {
        return 0;
    }
    let mut start = bytes.len() - keep;
    while start < bytes.len() && bytes[start] & 0b1100_0000 == 0b1000_0000 {
        start += 1;
    }
    start
}

/// Cut `value` down to at most `max_bytes`, on a char boundary. No
/// marker: the caller either does not need one or appends its own.
pub fn truncate_bytes(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }
    let end = floor_boundary(value, max_bytes);
    value.truncate(end);
}

/// Cut `value` down to at most `max_bytes` *including* a trailing `…`,
/// which is appended only when something was actually dropped.
pub fn truncate_bytes_ellipsis(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }
    let end = floor_boundary(value, max_bytes.saturating_sub(ELLIPSIS.len_utf8()));
    value.truncate(end);
    value.push(ELLIPSIS);
}

/// The first `max_chars` characters of `value`.
pub fn truncate_chars(value: &str, max_chars: usize) -> &str {
    match value.char_indices().nth(max_chars) {
        Some((index, _)) => &value[..index],
        None => value,
    }
}

/// The first `max_chars` characters of `value`, with `…` appended when
/// that dropped anything. The ellipsis is *extra*: the result is at most
/// `max_chars + 1` characters.
pub fn truncate_chars_ellipsis(value: &str, max_chars: usize) -> String {
    let kept = truncate_chars(value, max_chars);
    if kept.len() == value.len() {
        return value.to_string();
    }
    format!("{kept}{ELLIPSIS}")
}

/// The last `keep` bytes of `bytes`, starting on a UTF-8 boundary.
pub fn tail_bytes(bytes: &[u8], keep: usize) -> &[u8] {
    &bytes[tail_start(bytes, keep)..]
}

/// The last `keep` bytes of `value`, starting on a char boundary.
pub fn tail_str(value: &str, keep: usize) -> &str {
    &value[tail_start(value.as_bytes(), keep)..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_truncation_backs_off_a_split_codepoint() {
        let mut value = "aé".to_string(); // 61 c3 a9
        truncate_bytes(&mut value, 2);
        assert_eq!(value, "a");
        let mut exact = "aé".to_string();
        truncate_bytes(&mut exact, 3);
        assert_eq!(exact, "aé");
    }

    #[test]
    fn byte_truncation_is_a_noop_under_the_cap_and_on_empty_input() {
        let mut short = "hi".to_string();
        truncate_bytes(&mut short, 100);
        assert_eq!(short, "hi");
        let mut empty = String::new();
        truncate_bytes(&mut empty, 0);
        assert_eq!(empty, "");
        truncate_bytes_ellipsis(&mut empty, 0);
        assert_eq!(empty, "");
    }

    #[test]
    fn byte_ellipsis_fits_inside_the_cap() {
        let mut value = "abcdef".to_string();
        truncate_bytes_ellipsis(&mut value, 5);
        assert_eq!(value, "ab…");
        assert_eq!(value.len(), 5);
    }

    /// A cap smaller than `…` itself must not panic or overshoot into a
    /// codepoint: it degrades to the marker alone.
    #[test]
    fn byte_ellipsis_survives_a_cap_below_its_own_width() {
        let mut value = "abcdef".to_string();
        truncate_bytes_ellipsis(&mut value, 1);
        assert_eq!(value, "…");
    }

    #[test]
    fn byte_ellipsis_leaves_a_string_that_exactly_fills_the_cap() {
        let mut value = "abcde".to_string();
        truncate_bytes_ellipsis(&mut value, 5);
        assert_eq!(value, "abcde");
    }

    #[test]
    fn char_truncation_counts_codepoints_not_bytes() {
        assert_eq!(truncate_chars("ééé", 2), "éé");
        assert_eq!(truncate_chars("ééé", 3), "ééé");
        assert_eq!(truncate_chars("ééé", 9), "ééé");
        assert_eq!(truncate_chars("", 3), "");
        assert_eq!(truncate_chars("abc", 0), "");
    }

    #[test]
    fn char_ellipsis_is_added_on_top_of_the_cap_and_only_when_cutting() {
        assert_eq!(truncate_chars_ellipsis("abc", 3), "abc");
        assert_eq!(truncate_chars_ellipsis("abcd", 3), "abc…");
        assert_eq!(truncate_chars_ellipsis("", 0), "");
        assert_eq!(truncate_chars_ellipsis("éx", 1), "é…");
    }

    #[test]
    fn tail_starts_on_a_utf8_boundary() {
        let bytes = "aé".as_bytes(); // 61 c3 a9
        assert_eq!(tail_bytes(bytes, 3), bytes);
        assert_eq!(tail_bytes(bytes, 2), "é".as_bytes());
        // Cutting inside the codepoint skips its stray continuation byte.
        assert_eq!(tail_bytes(bytes, 1), b"");
        assert_eq!(tail_bytes(b"", 4), b"");
    }

    #[test]
    fn str_tail_matches_the_byte_tail() {
        assert_eq!(tail_str("aé", 2), "é");
        assert_eq!(tail_str("aé", 1), "");
        assert_eq!(tail_str("abc", 10), "abc");
        assert_eq!(tail_str("", 10), "");
    }
}
