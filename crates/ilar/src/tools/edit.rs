//! edit: exact-match string replacement. Errors on zero or multiple
//! matches unless replace_all.

use serde::Deserialize;

use super::{
    Tool, ToolConcurrency, ToolContext, ToolFuture, ToolOutput, WorkspaceAccess, WorkspaceCoverage,
    parse_input, run_blocking_io,
};

/// Edits load the whole file plus a rewritten copy, so the resident cost
/// is a few times this. Generous enough for lockfiles and fixtures.
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

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
        "Replace text in a file. old_string must match exactly once unless \
         replace_all is true. Include surrounding lines to disambiguate."
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
            let result =
                run_blocking_io(lease, move || replace_in_file(&path, &input, &cancel)).await;

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
    let matches = content.matches(&input.old_string).count();
    let (new_content, replacements) = match (matches, input.replace_all) {
        (0, _) => return Err(std::io::Error::other("old_string not found")),
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
    Ok(replacements)
}
