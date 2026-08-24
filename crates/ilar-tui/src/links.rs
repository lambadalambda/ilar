//! Link extraction from transcript lines, for the open-link picker.
//!
//! Transcript text is model-authored, so only `http`/`https` URLs are
//! collected: handing `file:` or custom schemes to the OS opener is an
//! attack surface, not a feature.

use crate::transcript::Line_;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LinkEntry {
    /// Markdown label when the link had one, else the URL itself.
    pub(crate) label: String,
    pub(crate) url: String,
}

/// All openable links in the transcript, newest first, deduplicated by
/// URL (the newest occurrence wins, and a labelled occurrence beats a
/// bare one at the same recency).
pub(crate) fn collect_links(lines: &[Line_]) -> Vec<LinkEntry> {
    let mut found = Vec::new();
    for line in lines {
        collect_from_line(line, &mut found);
    }
    found.reverse();
    // Newest occurrence wins the position; the newest *labelled*
    // occurrence wins the label, so a bare repost doesn't strip it.
    let mut labels = std::collections::HashMap::new();
    for entry in &found {
        if entry.label != entry.url {
            labels
                .entry(entry.url.clone())
                .or_insert(entry.label.clone());
        }
    }
    let mut seen = std::collections::HashSet::new();
    found.retain(|entry| seen.insert(entry.url.clone()));
    for entry in &mut found {
        if entry.label == entry.url
            && let Some(label) = labels.get(&entry.url)
        {
            entry.label = label.clone();
        }
    }
    found
}

fn collect_from_line(line: &Line_, out: &mut Vec<LinkEntry>) {
    match line {
        Line_::User(text) | Line_::Assistant(text) | Line_::System(text) => {
            links_in(text, out);
        }
        Line_::Task { text, .. } | Line_::Job { text, .. } | Line_::Thought { text, .. } => {
            links_in(text, out);
        }
        Line_::Tool {
            arguments,
            result,
            tail,
            child_lines,
            ..
        } => {
            links_in(arguments, out);
            if let Some(result) = result {
                links_in(result, out);
            }
            links_in(tail, out);
            for child in child_lines {
                collect_from_line(child, out);
            }
        }
    }
}

fn is_http_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// Whitespace and control characters have no place in a URL that will
/// become an argv (and, for control characters, a rendered row): the
/// bare-URL scanner terminates on them, and the markdown branch and
/// the opener enforce the same rule explicitly.
fn is_clean_url(url: &str) -> bool {
    !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Scan one text for markdown links and bare URLs, in order. A URL
/// inside a markdown link is not collected twice.
pub(crate) fn links_in(text: &str, out: &mut Vec<LinkEntry>) {
    let mut rest = text;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('[')
            && let Some(label_end) = after.find("](")
            && let Some(url_end) = after[label_end + 2..].find(')')
        {
            let url = &after[label_end + 2..label_end + 2 + url_end];
            if is_http_url(url) && is_clean_url(url) {
                let label = after[..label_end].trim();
                out.push(LinkEntry {
                    label: if label.is_empty() { url } else { label }.to_string(),
                    url: url.to_string(),
                });
                rest = &after[label_end + 2 + url_end + 1..];
                continue;
            }
        }
        if rest.starts_with("http://") || rest.starts_with("https://") {
            let scheme_len = if rest.starts_with("https") { 8 } else { 7 };
            let end = rest
                .find(|c: char| {
                    c.is_whitespace() || c.is_control() || matches!(c, '<' | '>' | '"' | '\'' | '`')
                })
                .unwrap_or(rest.len());
            let url = trim_trailing_punctuation(&rest[..end]);
            if url.len() > scheme_len {
                out.push(LinkEntry {
                    label: url.to_string(),
                    url: url.to_string(),
                });
            }
            // `end` >= scheme_len here, so this always advances.
            rest = &rest[end..];
            continue;
        }
        let next = rest
            .char_indices()
            .skip(1)
            .find(|(_, c)| matches!(c, '[' | 'h'))
            .map(|(index, _)| index)
            .unwrap_or(rest.len());
        rest = &rest[next..];
    }
}

/// Sentence punctuation clinging to a pasted URL is not part of it; a
/// closing paren is kept only when the URL contains an opening one
/// (Wikipedia-style paths).
fn trim_trailing_punctuation(url: &str) -> &str {
    let mut url = url;
    loop {
        let trimmed = url.trim_end_matches(['.', ',', ';', ':', '!', '?']);
        let trimmed = match trimmed.strip_suffix(')') {
            Some(without) if !trimmed.contains('(') => without,
            _ => trimmed,
        };
        if trimmed.len() == url.len() {
            return url;
        }
        url = trimmed;
    }
}

/// Open a collected URL with the platform opener, detached. The
/// scheme is re-checked at the door: the picker only holds http(s)
/// entries, but this is the last line before an exec.
pub(crate) fn open_in_browser(url: &str) -> std::io::Result<()> {
    if !is_http_url(url) || !is_clean_url(url) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "only http(s) links can be opened",
        ));
    }
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(lines: &[Line_]) -> Vec<String> {
        collect_links(lines)
            .into_iter()
            .map(|entry| entry.url)
            .collect()
    }

    #[test]
    fn markdown_links_and_bare_urls_collect_newest_first() {
        let lines = vec![
            Line_::Assistant(
                "PR opened: [yodlpay/yodl-native-app#444](https://github.com/yodlpay/yodl-native-app/pull/444)".into(),
            ),
            Line_::User("see https://example.com/docs.".into()),
        ];
        let links = collect_links(&lines);
        assert_eq!(
            links,
            vec![
                LinkEntry {
                    label: "https://example.com/docs".into(),
                    url: "https://example.com/docs".into(),
                },
                LinkEntry {
                    label: "yodlpay/yodl-native-app#444".into(),
                    url: "https://github.com/yodlpay/yodl-native-app/pull/444".into(),
                },
            ]
        );
    }

    #[test]
    fn duplicate_urls_keep_the_newest_occurrence() {
        let lines = vec![
            Line_::Assistant("[first mention](https://example.com/a)".into()),
            Line_::Assistant("later: https://example.com/a and https://example.com/b".into()),
        ];
        assert_eq!(
            urls(&lines),
            vec!["https://example.com/b", "https://example.com/a"]
        );
    }

    #[test]
    fn non_http_schemes_are_never_collected() {
        let lines = vec![Line_::Assistant(
            "[evil](file:///etc/passwd) [also](javascript:alert(1)) ftp://x mailto:a@b https://ok.example".into(),
        )];
        assert_eq!(urls(&lines), vec!["https://ok.example"]);
    }

    #[test]
    fn trailing_punctuation_is_trimmed_but_wiki_parens_survive() {
        let mut out = Vec::new();
        links_in(
            "read https://en.wikipedia.org/wiki/Rust_(language) now",
            &mut out,
        );
        links_in("or (https://example.com/plain).", &mut out);
        links_in("also <https://example.com/angle>!", &mut out);
        let urls: Vec<&str> = out.iter().map(|entry| entry.url.as_str()).collect();
        assert_eq!(
            urls,
            vec![
                "https://en.wikipedia.org/wiki/Rust_(language)",
                "https://example.com/plain",
                "https://example.com/angle",
            ]
        );
    }

    #[test]
    fn tool_results_and_nested_children_are_scanned() {
        let child = Line_::Assistant("nested https://example.com/child".into());
        let lines = vec![Line_::Tool {
            id: "t1".into(),
            group_id: "g1".into(),
            name: "websearch".into(),
            kind: crate::transcript::ToolKind::Tool,
            arguments: "{\"url\": \"https://example.com/arg\"}".into(),
            argument_detail: String::new(),
            diff: Vec::new(),
            tail: String::new(),
            result: Some("hit: https://example.com/result".into()),
            state: crate::transcript::ToolState::Succeeded,
            progress: crate::transcript::ToolProgress::None,
            expanded: false,
            full: false,
            child_lines: vec![child],
            child_group: 0,
            child_running: false,
            child_session_id: None,
        }];
        let mut found = urls(&lines);
        found.sort();
        assert_eq!(
            found,
            vec![
                "https://example.com/arg",
                "https://example.com/child",
                "https://example.com/result",
            ]
        );
    }

    #[test]
    fn urls_with_whitespace_or_control_characters_never_reach_the_opener() {
        let mut out = Vec::new();
        links_in(
            "[x](https://evil.example/a b) [y](https://evil.example/\x1b]0;t\x07)",
            &mut out,
        );
        links_in("bare https://ok.example/path\x1b]0;owned\x07tail", &mut out);
        // A dirty markdown URL is rejected as written; the bare scanner
        // may salvage a clean prefix, which is fine — the property is
        // that nothing collected carries whitespace or control bytes.
        let urls: Vec<&str> = out.iter().map(|entry| entry.url.as_str()).collect();
        assert_eq!(
            urls,
            vec![
                "https://evil.example/a",
                "https://evil.example/",
                "https://ok.example/path",
            ]
        );
        assert!(out.iter().all(|entry| super::is_clean_url(&entry.url)));

        assert!(open_in_browser("file:///etc/passwd").is_err());
        assert!(open_in_browser("https://evil.example/a b").is_err());
        assert!(open_in_browser("https://evil.example/\x1b").is_err());
    }

    #[test]
    fn a_labelled_mention_keeps_its_label_through_a_bare_repost() {
        let lines = vec![Line_::Assistant(
            "[the PR](https://example.com/pr) … later https://example.com/pr again".into(),
        )];
        assert_eq!(
            collect_links(&lines),
            vec![LinkEntry {
                label: "the PR".into(),
                url: "https://example.com/pr".into(),
            }]
        );
    }

    #[test]
    fn a_bare_scheme_with_nothing_after_it_is_ignored() {
        let mut out = Vec::new();
        links_in("https:// is how urls start", &mut out);
        assert!(out.is_empty());
    }
}
