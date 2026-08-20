//! Commands: markdown whose body *is* the prompt.
//!
//! The counterpart to skills. A skill is listed in the system prompt
//! with cue phrases and loaded through a tool when the model decides it
//! applies; a command is only ever invoked by the user as `/name args`,
//! and its body is submitted directly. No round trip, no discretion.

use std::path::PathBuf;

use anyhow::Context;

#[derive(Debug, Clone, PartialEq)]
pub struct Command {
    pub name: String,
    pub description: String,
    /// The prompt, before argument substitution.
    pub template: String,
    /// Optional overrides — see meta/issues/honour-command-frontmatter.md.
    pub agent: Option<String>,
    pub model: Option<String>,
    pub variant: Option<String>,
    /// Run as a background subagent instead of a main-session turn.
    pub subtask: bool,
}

/// Split arguments the way a shell would for `$1`, `$2`: whitespace
/// separated, with quoted runs held together.
fn positional(args: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    for character in args.chars() {
        match (quote, character) {
            (Some(open), c) if c == open => quote = None,
            (Some(_), c) => current.push(c),
            // Only at the start of a token: an apostrophe inside a word
            // is an apostrophe. `don't fix it` is three arguments, not
            // one unterminated quote that eats the rest.
            (None, c @ ('"' | '\'')) if current.is_empty() && !started => {
                started = true;
                quote = Some(c);
            }
            (None, c) if c.is_whitespace() => {
                if started || !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                }
                started = false;
            }
            (None, c) => {
                started = true;
                current.push(c);
            }
        }
    }
    if started || !current.is_empty() {
        parts.push(current);
    }
    parts
}

/// Substitute `$ARGUMENTS` and `$1`..`$9`. Anything else beginning with
/// `$` is left exactly as written — command bodies contain shell
/// snippets and prices, and mangling those would be worse than not
/// substituting at all.
pub fn expand(template: &str, args: &str) -> String {
    let parts = positional(args);
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(index) = rest.find('$') {
        out.push_str(&rest[..index]);
        let tail = &rest[index + 1..];
        if let Some(remainder) = tail.strip_prefix("ARGUMENTS")
            && !remainder
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric() || c == '_')
        {
            out.push_str(args);
            rest = remainder;
            continue;
        }
        // Only a single digit: `$10` is `$1` followed by a literal 0,
        // which is what shells do and what command authors expect.
        match tail.chars().next().and_then(|c| c.to_digit(10)) {
            Some(position) if position >= 1 => {
                out.push_str(
                    parts
                        .get(position as usize - 1)
                        .map(String::as_str)
                        .unwrap_or(""),
                );
                rest = &tail[1..];
            }
            _ => {
                out.push('$');
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

fn parse_command_md(name: &str, text: &str) -> anyhow::Result<Command> {
    let (frontmatter, body) = crate::config::split_frontmatter(text)?;
    let fm =
        crate::config::parse_frontmatter(&frontmatter).context("invalid command frontmatter")?;
    let template = body.trim_start_matches('\n').to_string();
    anyhow::ensure!(
        !template.trim().is_empty(),
        "command body is empty; the body below the frontmatter is the prompt"
    );
    let name = fm.name.unwrap_or_else(|| name.into());
    anyhow::ensure!(
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "command name {name:?} cannot be invoked: /name accepts letters, digits, - and _"
    );
    // `/goal` is built in and handled before commands, so a command by
    // that name could never run. Say so rather than listing something
    // that silently does something else.
    anyhow::ensure!(
        name != "goal",
        "command name \"goal\" is reserved: /goal is built in"
    );
    Ok(Command {
        description: fm.description.unwrap_or_else(|| name.clone()),
        name,
        template,
        agent: fm.extras.get("agent").cloned(),
        model: fm.extras.get("model").cloned(),
        variant: fm.extras.get("variant").cloned(),
        subtask: fm
            .extras
            .get("subtask")
            .is_some_and(|value| value == "true"),
    })
}

pub struct CommandStore {
    user_dir: PathBuf,
    project_dir: PathBuf,
}

impl CommandStore {
    pub fn new(user_dir: PathBuf, project_dir: PathBuf) -> Self {
        Self {
            user_dir,
            project_dir,
        }
    }

    /// User dir then project `.ilar/commands` (later wins by name).
    pub fn list(&self) -> anyhow::Result<Vec<Command>> {
        let mut commands: Vec<Command> = Vec::new();
        for (dir, sub) in [
            (&self.user_dir, "commands"),
            (&self.project_dir, ".ilar/commands"),
        ] {
            let dir = dir.join(sub);
            for path in crate::config::markdown_files(&dir)? {
                let name = path
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .with_context(|| {
                        format!("command filename is not UTF-8: {}", path.display())
                    })?;
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("reading command {}", path.display()))?;
                let command = parse_command_md(name, &text)
                    .with_context(|| format!("parsing command {}", path.display()))?;
                commands.retain(|existing| existing.name != command.name);
                commands.push(command);
            }
        }
        commands.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(commands)
    }
}
