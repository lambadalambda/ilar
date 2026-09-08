//! AGENTS.md / CLAUDE.md discovery and system-prompt assembly.

use std::path::{Path, PathBuf};

use anyhow::Context;

const BASE_PROMPT: &str = "You are ilar, a terminal coding agent. You have \
tools: read, write, edit, bash, glob, grep. Work in the user's project \
directory. Be terse; verify assumptions against the actual source before \
acting; prefer minimal diffs. When several tool calls are independent, make \
them in one response: every response re-reads the whole conversation, so a \
turn costs what its response count costs, not its tool count. When a task \
is done, stop.";

/// Whether the working directory's own context file is used for this
/// launch. It is unauthenticated third-party input — often a year
/// stale, occasionally hostile — so a launch can leave it out without
/// touching the file. The user's own context is never affected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProjectInstructions {
    #[default]
    Include,
    Skip,
}

/// An assembled system prompt, plus what assembling it left out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemPrompt {
    pub prompt: String,
    /// The name of the working directory's context file when one exists
    /// and was not used. Callers surface it: instructions dropped
    /// without a word read as a bug in the program rather than as a
    /// rule about the flag.
    pub skipped_project_file: Option<&'static str>,
}

/// The context files a coding session reads, first found wins.
pub const CONTEXT_FILES: &[&str] = &["AGENTS.md", "CLAUDE.md"];

/// The same for an assistant: a `SOUL.md` — who it is, how it talks —
/// beats the coding instructions when there is one.
pub const SOUL_FILES: &[&str] = &["SOUL.md", "AGENTS.md", "CLAUDE.md"];

/// Assemble the system prompt from the user config directory and exact working
/// directory. AGENTS.md wins over CLAUDE.md within each location.
pub fn system_prompt_for(
    user_config_dir: &Path,
    cwd: &Path,
    project: ProjectInstructions,
) -> anyhow::Result<SystemPrompt> {
    system_prompt_with(user_config_dir, cwd, project, CONTEXT_FILES)
}

/// [`system_prompt_for`] reading the first of `names` present in each
/// location.
pub fn system_prompt_with(
    user_config_dir: &Path,
    cwd: &Path,
    project: ProjectInstructions,
    names: &[&str],
) -> anyhow::Result<SystemPrompt> {
    let skip = project == ProjectInstructions::Skip;
    let mut locations = vec![("User context", user_config_dir)];
    if !skip {
        locations.push(("Working directory context", cwd));
    }

    let mut prompt = BASE_PROMPT.to_string();
    for (label, dir) in locations {
        if let Some((path, content)) = context_file_in(dir, names)? {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("AGENTS.md");
            prompt.push_str(&format!(
                "\n\n# {label} (from {name}):\n\n{}",
                content.trim()
            ));
        }
    }
    Ok(SystemPrompt {
        prompt,
        skipped_project_file: skip.then(|| context_file_present(cwd, names)).flatten(),
    })
}

/// Which context file the directory has, if any. Used on the skip path,
/// which must not open it: a project `AGENTS.md` is unauthenticated
/// third-party input, and a launch that refuses it should not fail on
/// it either — so this only asks whether it is there.
fn context_file_present(dir: &Path, names: &[&str]) -> Option<&'static str> {
    // The names are the two static lists above; anything else is
    // reported by its list's first static match, which is close enough
    // for a skipped file's name.
    CONTEXT_FILES
        .iter()
        .chain(SOUL_FILES.iter())
        .copied()
        .find(|name| names.contains(name) && dir.join(name).is_file())
}

fn context_file_in(dir: &Path, names: &[&str]) -> anyhow::Result<Option<(PathBuf, String)>> {
    for name in names {
        let path = dir.join(name);
        match std::fs::read_to_string(&path) {
            Ok(content) => return Ok(Some((path, content))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("reading context {}", path.display()));
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A user config dir with `AGENTS.md` and a working directory with
    /// its own: the two locations the prompt is assembled from.
    fn two_locations() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let guard = tempfile::tempdir().unwrap();
        let user = guard.path().join("config");
        let cwd = guard.path().join("project");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(user.join("AGENTS.md"), "user rules\n").unwrap();
        std::fs::write(cwd.join("AGENTS.md"), "project rules\n").unwrap();
        (guard, user, cwd)
    }

    /// Each response re-reads the whole context, so a turn's cost is its
    /// request count; the prompt has to say so or the model serializes
    /// reads it could have made together (measured 1.08 calls per request
    /// for the build agent, 2026-09-05..07).
    #[test]
    fn the_base_prompt_asks_for_independent_calls_in_one_response() {
        let (_guard, user, cwd) = two_locations();

        let assembled = system_prompt_for(&user, &cwd, ProjectInstructions::Include).unwrap();

        assert!(
            assembled
                .prompt
                .contains("When several tool calls are independent, make them in one response"),
            "{assembled:?}"
        );
    }

    /// An assistant asks for a SOUL.md first; without one, the coding
    /// instructions stand, and a coding session never reads a SOUL.md.
    #[test]
    fn a_soul_file_is_preferred_only_when_asked_for() {
        let (_guard, user, cwd) = two_locations();
        std::fs::write(user.join("SOUL.md"), "you are Sprocket\n").unwrap();

        let soul =
            system_prompt_with(&user, &cwd, ProjectInstructions::Include, SOUL_FILES).unwrap();
        assert!(soul.prompt.contains("you are Sprocket"), "{soul:?}");
        assert!(!soul.prompt.contains("user rules"), "{soul:?}");
        assert!(soul.prompt.contains("project rules"), "{soul:?}");

        let coding = system_prompt_for(&user, &cwd, ProjectInstructions::Include).unwrap();
        assert!(!coding.prompt.contains("Sprocket"), "{coding:?}");
        assert!(coding.prompt.contains("user rules"), "{coding:?}");

        std::fs::remove_file(user.join("SOUL.md")).unwrap();
        let fallback =
            system_prompt_with(&user, &cwd, ProjectInstructions::Include, SOUL_FILES).unwrap();
        assert!(fallback.prompt.contains("user rules"), "{fallback:?}");
    }

    #[test]
    fn project_instructions_are_included_by_default() {
        let (_guard, user, cwd) = two_locations();

        let assembled = system_prompt_for(&user, &cwd, ProjectInstructions::Include).unwrap();

        assert!(assembled.prompt.contains("user rules"), "{assembled:?}");
        assert!(assembled.prompt.contains("project rules"), "{assembled:?}");
        assert_eq!(assembled.skipped_project_file, None);
    }

    #[test]
    fn skipping_drops_the_working_directory_section_and_keeps_user_context() {
        let (_guard, user, cwd) = two_locations();

        let assembled = system_prompt_for(&user, &cwd, ProjectInstructions::Skip).unwrap();

        assert!(assembled.prompt.contains("user rules"), "{assembled:?}");
        assert!(!assembled.prompt.contains("project rules"), "{assembled:?}");
        assert!(
            !assembled.prompt.contains("Working directory context"),
            "{assembled:?}"
        );
        // The caller has to be able to say so, by name: silently
        // dropping a file the project put there reads as a bug, not as
        // a rule.
        assert_eq!(assembled.skipped_project_file, Some("AGENTS.md"));
    }

    /// Reported under the name it actually has: telling someone their
    /// AGENTS.md was skipped when they wrote a CLAUDE.md sends them
    /// looking for a file that is not there.
    #[test]
    fn the_skipped_file_is_named_as_written() {
        let guard = tempfile::tempdir().unwrap();
        let user = guard.path().join("config");
        let cwd = guard.path().join("project");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(cwd.join("CLAUDE.md"), "project rules\n").unwrap();

        let assembled = system_prompt_for(&user, &cwd, ProjectInstructions::Skip).unwrap();

        assert_eq!(assembled.skipped_project_file, Some("CLAUDE.md"));
    }

    #[test]
    fn nothing_is_reported_skipped_when_the_project_has_no_file() {
        let guard = tempfile::tempdir().unwrap();
        let user = guard.path().join("config");
        let cwd = guard.path().join("project");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(user.join("AGENTS.md"), "user rules\n").unwrap();

        let assembled = system_prompt_for(&user, &cwd, ProjectInstructions::Skip).unwrap();

        assert_eq!(assembled.skipped_project_file, None);
        assert!(assembled.prompt.contains("user rules"), "{assembled:?}");
    }

    /// The point of the flag is to not consume the file at all, so a
    /// project file that cannot even be read must not fail the launch
    /// that refused it — while it still fails the launch that wants it.
    #[test]
    fn a_skipped_project_file_is_never_read() {
        let guard = tempfile::tempdir().unwrap();
        let user = guard.path().join("config");
        let cwd = guard.path().join("project");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(cwd.join("AGENTS.md"), [0xff]).unwrap();

        let assembled = system_prompt_for(&user, &cwd, ProjectInstructions::Skip).unwrap();
        assert_eq!(assembled.skipped_project_file, Some("AGENTS.md"));

        assert!(system_prompt_for(&user, &cwd, ProjectInstructions::Include).is_err());
    }
}
