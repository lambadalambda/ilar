//! write: create/overwrite a file, creating parent directories.

use serde::Deserialize;

use super::{Tool, ToolContext, ToolFuture, ToolKind, ToolOutput, parse_input};

pub struct WriteTool;

#[derive(Deserialize)]
struct Input {
    path: String,
    content: String,
}

impl Tool for WriteTool {
    fn name(&self) -> &'static str {
        "write"
    }

    fn description(&self) -> &'static str {
        "Create or overwrite a file with the given content. Parent \
         directories are created as needed."
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Mutating
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "content": {"type": "string"}
            },
            "required": ["path", "content"]
        })
    }

    fn run(&self, input: serde_json::Value, ctx: ToolContext) -> ToolFuture {
        Box::pin(async move {
            let input: Input = match parse_input(input, "write") {
                Ok(v) => v,
                Err(e) => return e,
            };
            let path = ctx.cwd.join(&input.path);
            if let Some(parent) = path.parent()
                && let Err(e) = std::fs::create_dir_all(parent)
            {
                return ToolOutput::error(format!("write {}: {e}", input.path));
            }
            match crate::atomic_file::replace(
                &path,
                input.content.as_bytes(),
                crate::atomic_file::Mode::Preserve,
            ) {
                Ok(()) => ToolOutput::text(format!(
                    "wrote {} ({} bytes)",
                    input.path,
                    input.content.len()
                )),
                Err(e) => ToolOutput::error(format!("write {}: {e}", input.path)),
            }
        })
    }
}
