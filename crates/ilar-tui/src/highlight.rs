//! Dependency-free per-line syntax highlighting for fenced code blocks —
//! see meta/issues/code-fence-syntax-highlighting.md.
//!
//! Line-scoped tokenizing (strings, comments, numbers, keywords) with one
//! bit of cross-line state (block comments). Deliberately approximate:
//! wrong colors degrade gracefully, and unknown languages stay plain.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Rust,
    /// C-family line/block comment languages: JS/TS, Go, C/C++, Java.
    CLike,
    Python,
    Shell,
    Json,
    Toml,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Keyword,
    String,
    Comment,
    Number,
    Plain,
}

/// Cross-line tokenizer state within one fenced block.
#[derive(Debug, Clone, Copy, Default)]
pub struct BlockState {
    in_block_comment: bool,
}

pub fn language_for(info: &str) -> Option<Language> {
    let name = info
        .split(|character: char| character.is_whitespace() || character == ',')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match name.as_str() {
        "rust" | "rs" => Some(Language::Rust),
        "js" | "jsx" | "javascript" | "ts" | "tsx" | "typescript" | "go" | "c" | "cpp" | "c++"
        | "h" | "java" | "kotlin" | "swift" => Some(Language::CLike),
        "python" | "py" => Some(Language::Python),
        "sh" | "bash" | "zsh" | "shell" | "fish" | "console" => Some(Language::Shell),
        "json" | "jsonl" => Some(Language::Json),
        "toml" | "ini" => Some(Language::Toml),
        _ => None,
    }
}

const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type",
    "unsafe", "use", "where", "while",
];
const CLIKE_KEYWORDS: &[&str] = &[
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "default",
    "defer",
    "delete",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "func",
    "function",
    "go",
    "if",
    "implements",
    "import",
    "in",
    "instanceof",
    "interface",
    "let",
    "map",
    "new",
    "nil",
    "null",
    "package",
    "private",
    "public",
    "range",
    "return",
    "static",
    "struct",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "type",
    "typeof",
    "undefined",
    "var",
    "void",
    "while",
];
const PYTHON_KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];
const SHELL_KEYWORDS: &[&str] = &[
    "case", "do", "done", "elif", "else", "esac", "exit", "export", "fi", "for", "function", "if",
    "in", "local", "return", "set", "then", "while",
];
const JSON_KEYWORDS: &[&str] = &["true", "false", "null"];
const TOML_KEYWORDS: &[&str] = &["true", "false"];

impl Language {
    fn keywords(self) -> &'static [&'static str] {
        match self {
            Language::Rust => RUST_KEYWORDS,
            Language::CLike => CLIKE_KEYWORDS,
            Language::Python => PYTHON_KEYWORDS,
            Language::Shell => SHELL_KEYWORDS,
            Language::Json => JSON_KEYWORDS,
            Language::Toml => TOML_KEYWORDS,
        }
    }

    fn line_comment(self) -> Option<&'static str> {
        match self {
            Language::Rust | Language::CLike => Some("//"),
            Language::Python | Language::Shell | Language::Toml => Some("#"),
            Language::Json => None,
        }
    }

    fn block_comments(self) -> bool {
        matches!(self, Language::Rust | Language::CLike)
    }

    fn string_delimiters(self) -> &'static [char] {
        match self {
            Language::Rust | Language::Json => &['"'],
            Language::Toml => &['"', '\''],
            Language::Python | Language::Shell => &['"', '\''],
            Language::CLike => &['"', '\'', '`'],
        }
    }
}

/// Tokenize one line into `(class, text)` runs covering the entire input.
pub fn highlight_line(
    language: Language,
    line: &str,
    state: &mut BlockState,
) -> Vec<(Class, String)> {
    let mut runs: Vec<(Class, String)> = Vec::new();
    let mut push = |class: Class, text: &str| {
        if text.is_empty() {
            return;
        }
        if let Some((last_class, last_text)) = runs.last_mut()
            && *last_class == class
        {
            last_text.push_str(text);
        } else {
            runs.push((class, text.to_string()));
        }
    };
    let bytes = line.as_bytes();
    let mut position = 0;
    while position < line.len() {
        let rest = &line[position..];
        if state.in_block_comment {
            match rest.find("*/") {
                Some(end) => {
                    push(Class::Comment, &rest[..end + 2]);
                    position += end + 2;
                    state.in_block_comment = false;
                }
                None => {
                    push(Class::Comment, rest);
                    return runs;
                }
            }
            continue;
        }
        if language.block_comments() && rest.starts_with("/*") {
            state.in_block_comment = true;
            continue;
        }
        if let Some(marker) = language.line_comment()
            && rest.starts_with(marker)
        {
            push(Class::Comment, rest);
            return runs;
        }
        let character = rest.chars().next().expect("in-bounds char");
        if language.string_delimiters().contains(&character) {
            let mut end = position + character.len_utf8();
            let mut escaped = false;
            let mut closed = false;
            for (offset, found) in rest[character.len_utf8()..].char_indices() {
                if escaped {
                    escaped = false;
                } else if found == '\\' && language != Language::Toml {
                    escaped = true;
                } else if found == character {
                    end = position + character.len_utf8() + offset + found.len_utf8();
                    closed = true;
                    break;
                }
            }
            if !closed {
                end = line.len();
            }
            push(Class::String, &line[position..end]);
            position = end;
            continue;
        }
        if character.is_ascii_digit() && (position == 0 || !is_word_byte(bytes[position - 1])) {
            let end = position
                + rest
                    .find(|found: char| {
                        !(found.is_ascii_alphanumeric() || found == '.' || found == '_')
                    })
                    .unwrap_or(rest.len());
            push(Class::Number, &line[position..end]);
            position = end;
            continue;
        }
        if character.is_alphabetic() || character == '_' {
            let end = position
                + rest
                    .find(|found: char| !(found.is_alphanumeric() || found == '_'))
                    .unwrap_or(rest.len());
            let word = &line[position..end];
            let class = if language.keywords().contains(&word) {
                Class::Keyword
            } else {
                Class::Plain
            };
            push(class, word);
            position = end;
            continue;
        }
        push(Class::Plain, &rest[..character.len_utf8()]);
        position += character.len_utf8();
    }
    runs
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classes(language: Language, line: &str) -> Vec<(Class, String)> {
        highlight_line(language, line, &mut BlockState::default())
    }

    fn joined(runs: &[(Class, String)]) -> String {
        runs.iter().map(|(_, text)| text.as_str()).collect()
    }

    #[test]
    fn runs_cover_the_whole_line() {
        for (language, line) in [
            (Language::Rust, "let x = \"a \\\" b\"; // trailing"),
            (Language::Python, "def f(n):  # comment with 'quote"),
            (Language::Json, "{\"key\": [1, 2.5, true]}"),
            (Language::Shell, "echo \"$HOME\" # done"),
        ] {
            let runs = classes(language, line);
            assert_eq!(joined(&runs), line, "{language:?}");
        }
    }

    #[test]
    fn keywords_strings_comments_numbers_classified() {
        let runs = classes(Language::Rust, "let total = compute(42); // sum");
        assert!(runs.contains(&(Class::Keyword, "let".into())), "{runs:?}");
        assert!(runs.contains(&(Class::Number, "42".into())), "{runs:?}");
        assert!(
            runs.contains(&(Class::Comment, "// sum".into())),
            "{runs:?}"
        );
        assert!(
            runs.iter()
                .any(|(class, text)| *class == Class::Plain && text.contains("compute")),
            "{runs:?}"
        );

        let runs = classes(Language::Rust, "print(\"esc \\\" still string\") + 1");
        assert!(
            runs.contains(&(Class::String, "\"esc \\\" still string\"".into())),
            "{runs:?}"
        );

        // Identifier-embedded digits are not numbers.
        let runs = classes(Language::Rust, "base64::encode(x2)");
        assert!(
            !runs.iter().any(|(class, _)| *class == Class::Number),
            "{runs:?}"
        );
    }

    #[test]
    fn block_comments_span_lines() {
        let mut state = BlockState::default();
        let first = highlight_line(Language::Rust, "code(); /* open", &mut state);
        assert!(
            first.contains(&(Class::Comment, "/* open".into())),
            "{first:?}"
        );
        let second = highlight_line(Language::Rust, "still comment", &mut state);
        assert_eq!(second, vec![(Class::Comment, "still comment".into())]);
        let third = highlight_line(Language::Rust, "end */ after()", &mut state);
        assert!(
            third.contains(&(Class::Comment, "end */".into())),
            "{third:?}"
        );
        assert!(joined(&third).ends_with(" after()"));
        assert!(!state.in_block_comment);
    }

    #[test]
    fn unterminated_strings_extend_to_line_end_without_panicking() {
        let runs = classes(Language::Python, "s = 'unterminated");
        assert!(
            runs.contains(&(Class::String, "'unterminated".into())),
            "{runs:?}"
        );
        // JSON has no comments; a lone quote must still terminate.
        let runs = classes(Language::Json, "\"open");
        assert_eq!(runs, vec![(Class::String, "\"open".into())]);
    }

    #[test]
    fn language_detection_covers_aliases_and_unknowns() {
        assert_eq!(language_for("rust"), Some(Language::Rust));
        assert_eq!(language_for("ts"), Some(Language::CLike));
        assert_eq!(language_for("PYTHON"), Some(Language::Python));
        assert_eq!(language_for("bash session"), Some(Language::Shell));
        assert_eq!(language_for(""), None);
        assert_eq!(language_for("brainfuck"), None);
    }

    #[test]
    fn multibyte_content_is_boundary_safe() {
        let line = "let s = \"héllo — wörld\"; // ünicode";
        let runs = classes(Language::Rust, line);
        assert_eq!(joined(&runs), line);
    }
}
