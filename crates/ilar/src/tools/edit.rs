//! edit: exact-match string replacement. Errors on zero or multiple
//! matches unless replace_all.

use serde::Deserialize;

use super::{
    Tool, ToolConcurrency, ToolContext, ToolFuture, ToolOutput, WorkspaceAccess, parse_input,
};

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
            let path = ctx.cwd.join(&input.path);
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => return ToolOutput::error(format!("edit {}: {e}", input.path)),
            };
            if input.old_string == input.new_string {
                return ToolOutput::error("old_string and new_string are identical");
            }
            let matches = content.matches(&input.old_string).count();
            let new_content = match (matches, input.replace_all) {
                (0, _) => {
                    return ToolOutput::error(format!("edit {}: old_string not found", input.path));
                }
                (1, _) => content.replacen(&input.old_string, &input.new_string, 1),
                (_n, true) => content.replace(&input.old_string, &input.new_string),
                (n, false) => {
                    return ToolOutput::error(format!(
                        "edit {}: old_string matches {n} times; add surrounding context to make it unique, or set replace_all",
                        input.path
                    ));
                }
            };
            match crate::atomic_file::replace(
                &path,
                new_content.as_bytes(),
                crate::atomic_file::Mode::Preserve,
            ) {
                Ok(()) => ToolOutput::text(format!(
                    "edited {}: {} replacement{}",
                    input.path,
                    if input.replace_all { matches } else { 1 },
                    if input.replace_all && matches > 1 {
                        "s"
                    } else {
                        ""
                    }
                )),
                Err(e) => ToolOutput::error(format!("edit {}: {e}", input.path)),
            }
        })
    }
}
