//! Frontmatter shared by skills and commands.
//!
//! ilar writes TOML (`key = value`). Claude Code and opencode write
//! YAML (`key: value`), and reading their files unchanged is the point,
//! so both are accepted.
//!
//! The YAML side is a deliberate subset, not an implementation: flat
//! scalars, `- item` lists, block scalars, and nested blocks skipped
//! wholesale. Everything it does not understand is ignored rather than
//! guessed at, which is what lets a foreign file with unknown keys load.

use std::collections::BTreeMap;

use anyhow::Context;

/// Fields we understand, plus whatever else the file declared.
///
/// `extras` exists because the issues require unknown keys to be
/// preserved-and-ignored: a command's `model` or a skill's
/// `allowed-tools` should survive the parse even before anything
/// honours them.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Frontmatter {
    pub(crate) name: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) triggers: Vec<String>,
    pub(crate) extras: BTreeMap<String, String>,
}

impl Frontmatter {
    /// Blank is not a value: an empty description should fall back to
    /// the file name rather than render as nothing.
    fn set(&mut self, key: &str, value: String) {
        if value.trim().is_empty() {
            return;
        }
        match key {
            "name" => self.name = Some(value),
            "description" => self.description = Some(value),
            _ => {
                self.extras.insert(key.to_string(), value);
            }
        }
    }
}

/// TOML assigns with `=`, YAML with `:`. Probe the first meaningful
/// line rather than inferring from where the file lives, so a foreign
/// file dropped anywhere still loads.
///
/// The `=` must look like an assignment at the start of the line —
/// `description:set X=Y` is (invalid) YAML, not TOML, and treating it as
/// TOML would abort the whole load with a message naming the wrong
/// format.
fn looks_like_toml(frontmatter: &str) -> bool {
    for line in frontmatter.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let assignment = line
            .split_once('=')
            .is_some_and(|(key, _)| !key.trim().is_empty() && !key.contains(':'));
        return assignment || line.starts_with('[');
    }
    false
}

/// `|`, `>`, and their indented/chomping variants: `|-`, `>+`, `|2`,
/// `|2-`. Matching only the four bare forms left a description reading
/// literally `"|+"`, with the real text discarded.
fn block_scalar(value: &str) -> bool {
    let mut chars = value.chars();
    if !matches!(chars.next(), Some('|' | '>')) {
        return false;
    }
    let rest: String = chars.collect();
    let rest = rest.trim_end_matches(['-', '+']);
    rest.chars().all(|character| character.is_ascii_digit())
}

/// Split on the first `: ` (or a trailing `:`), so values containing
/// colons — `Bash(agent-browser:*)` — survive intact.
fn split_pair(line: &str) -> Option<(&str, &str)> {
    if let Some(index) = line.find(": ") {
        return Some((line[..index].trim(), line[index + 2..].trim()));
    }
    line.strip_suffix(':').map(|key| (key.trim(), ""))
}

/// Strip matching outer quotes, but only when they really wrap the
/// value: `"open a website" or "click a button"` is not a quoted
/// string, and unwrapping it eats the interior quotes.
fn unquote(value: &str) -> String {
    let value = value.trim();
    for quote in ['"', '\''] {
        if value.len() >= 2 && value.starts_with(quote) && value.ends_with(quote) {
            let inner = &value[1..value.len() - 1];
            if !inner.contains(quote) {
                return inner.to_string();
            }
        }
    }
    value.to_string()
}

fn indented(line: &str) -> bool {
    line.starts_with(' ') || line.starts_with('\t')
}

/// What an indented line belongs to.
enum Open {
    /// `key: |` — every indented line is content, including text that
    /// happens to look like `key: value`.
    Block(String, Vec<String>),
    /// `key: value` wrapped across lines, or `key:` with the value
    /// beneath. Ends at anything that looks structural.
    Value(String, Vec<String>),
    /// `key:` followed by `- item` lines.
    List(String),
    /// `key:` followed by `child: value` lines — skipped wholesale.
    Nested,
}

fn parse_yaml(frontmatter: &str) -> Frontmatter {
    let mut parsed = Frontmatter::default();
    let mut open: Option<Open> = None;

    fn flush(open: &mut Option<Open>, parsed: &mut Frontmatter) {
        match open.take() {
            Some(Open::Block(key, lines)) | Some(Open::Value(key, lines)) => {
                // Fold to one line: a description renders on a single
                // line of the system prompt either way, so `|` and `>`
                // are the same to us.
                let joined = lines.join(" ");
                parsed.set(
                    &key,
                    joined.split_whitespace().collect::<Vec<_>>().join(" "),
                );
            }
            _ => {}
        }
    }

    for line in frontmatter.lines() {
        let trimmed = line.trim();
        if indented(line) {
            match open.as_mut() {
                // Content wins over structure inside a block scalar: a
                // description may well contain "Triggers: foo".
                Some(Open::Block(_, lines)) => lines.push(trimmed.to_string()),
                Some(Open::List(key)) => {
                    if let Some(item) = trimmed.strip_prefix("- ")
                        && key == "triggers"
                    {
                        parsed.triggers.push(unquote(item));
                    }
                }
                Some(Open::Nested) => {}
                Some(Open::Value(_, lines)) => {
                    // A structural child means this was never a value.
                    if trimmed.starts_with("- ") {
                        let key = match open.take() {
                            Some(Open::Value(key, _)) => key,
                            _ => unreachable!(),
                        };
                        if key == "triggers" {
                            parsed
                                .triggers
                                .push(unquote(trimmed.trim_start_matches("- ")));
                        }
                        open = Some(Open::List(key));
                    } else if split_pair(trimmed).is_some() && lines.is_empty() {
                        open = Some(Open::Nested);
                    } else {
                        lines.push(trimmed.to_string());
                    }
                }
                None => {}
            }
            continue;
        }
        if trimmed.is_empty() {
            // A blank line is part of a block scalar, and ends anything
            // else that was open.
            match open.as_mut() {
                Some(Open::Block(_, lines)) => lines.push(String::new()),
                _ => flush(&mut open, &mut parsed),
            }
            continue;
        }
        flush(&mut open, &mut parsed);
        open = None;
        if trimmed.starts_with('#') {
            continue;
        }
        let Some((key, value)) = split_pair(trimmed) else {
            continue;
        };
        if block_scalar(value) {
            open = Some(Open::Block(key.to_string(), Vec::new()));
        } else if value.is_empty() {
            // Could be a list, a nested block, or a value on the next
            // line. The first child decides.
            open = Some(Open::Value(key.to_string(), Vec::new()));
        } else {
            parsed.set(key, unquote(value));
            open = Some(Open::Value(key.to_string(), vec![unquote(value)]));
        }
    }
    flush(&mut open, &mut parsed);
    parsed
}

pub(crate) fn parse(frontmatter: &str) -> anyhow::Result<Frontmatter> {
    if looks_like_toml(frontmatter) {
        let table: toml::Table = toml::from_str(frontmatter).context("invalid TOML frontmatter")?;
        let mut parsed = Frontmatter::default();
        for (key, value) in table {
            match (key.as_str(), &value) {
                ("triggers", toml::Value::Array(items)) => {
                    parsed.triggers = items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_string))
                        .collect();
                }
                (_, toml::Value::String(text)) => parsed.set(&key, text.clone()),
                _ => {
                    parsed.extras.insert(key, value.to_string());
                }
            }
        }
        return Ok(parsed);
    }
    Ok(parse_yaml(frontmatter))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yaml(text: &str) -> Frontmatter {
        parse(text).unwrap()
    }

    #[test]
    fn format_is_detected_from_the_first_meaningful_line() {
        assert!(looks_like_toml("description = \"x\""));
        assert!(looks_like_toml("# comment\n\ntriggers = [\"a\"]"));
        assert!(!looks_like_toml("description: x"));
        // A colon before the `=` means YAML, however invalid: routing it
        // to TOML aborts the load and blames the wrong format.
        assert!(!looks_like_toml("description:set X=Y in the environment"));
        assert!(!looks_like_toml("description: use a = b"));
    }

    #[test]
    fn block_scalar_indicators_are_recognised_in_full() {
        for indicator in ["|", ">", "|-", ">-", "|+", ">+", "|2", "|2-", ">3+"] {
            assert!(block_scalar(indicator), "{indicator}");
        }
        for plain in ["|x", "text", "", "a|b"] {
            assert!(!block_scalar(plain), "{plain}");
        }
    }

    #[test]
    fn a_block_scalar_survives_blank_lines() {
        let parsed = yaml("description: |\n  First paragraph.\n\n  Second paragraph.\n");
        assert_eq!(
            parsed.description.as_deref(),
            Some("First paragraph. Second paragraph.")
        );
    }

    #[test]
    fn a_chomped_block_scalar_keeps_its_text() {
        let parsed = yaml("description: |+\n  The real description.\n");
        assert_eq!(parsed.description.as_deref(), Some("The real description."));
    }

    #[test]
    fn a_wrapped_or_next_line_value_is_still_the_value() {
        let wrapped =
            yaml("description: A long description the author\n  wrapped onto two lines.\n");
        assert_eq!(
            wrapped.description.as_deref(),
            Some("A long description the author wrapped onto two lines.")
        );
        let next_line = yaml("description:\n  The whole description lives here.\n");
        assert_eq!(
            next_line.description.as_deref(),
            Some("The whole description lives here.")
        );
    }

    #[test]
    fn nested_blocks_are_skipped_without_overriding_the_top_level() {
        let parsed = yaml(
            "name: repo-issues\ndescription: Top level.\nmetadata:\n  category: workflow\n  description: nested and irrelevant\n",
        );
        assert_eq!(parsed.name.as_deref(), Some("repo-issues"));
        assert_eq!(parsed.description.as_deref(), Some("Top level."));
    }

    #[test]
    fn values_keep_their_colons_and_interior_quotes() {
        let parsed = yaml(
            "description: Use when the user says \"open a website\": drive it.\nallowed-tools: Bash(agent-browser:*)\n",
        );
        assert_eq!(
            parsed.description.as_deref(),
            Some("Use when the user says \"open a website\": drive it.")
        );
        assert_eq!(
            parsed.extras.get("allowed-tools").map(String::as_str),
            Some("Bash(agent-browser:*)")
        );
    }

    #[test]
    fn a_quoted_value_is_unwrapped_only_when_the_quotes_wrap_it() {
        let parsed = yaml("description: \"quoted\"\nname: \"a\" or \"b\"\n");
        assert_eq!(parsed.description.as_deref(), Some("quoted"));
        assert_eq!(parsed.name.as_deref(), Some("\"a\" or \"b\""));
    }

    #[test]
    fn an_empty_value_is_no_value_so_the_filename_can_win() {
        assert_eq!(yaml("description: |\n").description, None);
        assert_eq!(yaml("name: \"\"\ndescription: x").name, None);
    }

    #[test]
    fn triggers_load_from_both_list_and_toml_array() {
        let listed = yaml("triggers:\n  - search the web\n  - find articles\n");
        assert_eq!(listed.triggers, vec!["search the web", "find articles"]);
        let toml = parse("triggers = [\"cue\"]\ndescription = \"x\"").unwrap();
        assert_eq!(toml.triggers, vec!["cue"]);
    }

    #[test]
    fn unknown_toml_keys_are_kept_rather_than_rejected() {
        let parsed = parse("description = \"x\"\nmodel = \"zai/glm-4.7\"").unwrap();
        assert_eq!(parsed.description.as_deref(), Some("x"));
        assert_eq!(
            parsed.extras.get("model").map(String::as_str),
            Some("zai/glm-4.7")
        );
    }
}
