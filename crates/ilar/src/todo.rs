//! Session-scoped todo list tool — single `todo` write-tool rendering a
//! checklist (Claude Code todowrite style). Calls form an executor barrier so
//! replacements are deterministic in provider call order.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::tools::{Tool, ToolConcurrency, ToolContext, ToolFuture, ToolOutput, WorkspaceAccess};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    #[default]
    Pending,
    InProgress,
    Completed,
}

impl Status {
    fn marker(&self) -> &'static str {
        match self {
            Status::Pending => "[ ]",
            Status::InProgress => "[>]",
            Status::Completed => "[x]",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoList {
    pub items: Vec<TodoItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: Status,
}

impl TodoList {
    /// The checklist as the model wrote it and reads it back. Shared
    /// with compaction, which pins the list into its summary: the list
    /// lives in tool results, so a compaction that dropped them would
    /// leave the model with no evidence its own plan exists.
    pub fn checklist(&self) -> String {
        self.items
            .iter()
            .map(|item| format!("{} {}", item.status.marker(), item.content))
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self
            .items
            .iter()
            .filter(|item| item.status == Status::InProgress)
            .count()
            > 1
        {
            return Err("at most one todo item may be in_progress at a time");
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct Input {
    /// Absent means "read": the model has no other way to see its own
    /// plan, since the list lives in tool results that compaction
    /// eventually drops.
    todos: Option<Vec<ItemInput>>,
}

#[derive(Deserialize)]
struct ItemInput {
    content: String,
    status: String,
}

pub struct TodoTool {
    list: Arc<Mutex<TodoList>>,
}

impl TodoTool {
    pub fn new(list: Arc<Mutex<TodoList>>) -> Self {
        Self { list }
    }
}

impl Tool for TodoTool {
    fn name(&self) -> &'static str {
        "todo"
    }

    fn description(&self) -> &'static str {
        "Read or write the todo list for the current task. Call with no arguments to read \
         the current list — do that after a handover summary, or whenever you are unsure \
         what you were doing. Passing `todos` replaces the list entirely; exactly one item \
         may be in_progress."
    }

    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Barrier
    }

    fn workspace_access(&self) -> WorkspaceAccess {
        WorkspaceAccess::None
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": ["array", "null"],
                    "description": "The full replacement list. Omit to read the current one.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {"type": "string"},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]}
                        },
                        "required": ["content", "status"]
                    }
                }
            }
        })
    }

    fn run(&self, input: serde_json::Value, _ctx: ToolContext) -> ToolFuture {
        let list = self.list.clone();
        Box::pin(async move {
            let input: Input = match serde_json::from_value(input) {
                Ok(v) => v,
                Err(e) => return ToolOutput::error(format!("invalid input for todo: {e}")),
            };
            let Some(todos) = input.todos else {
                let current = list.lock().unwrap().checklist();
                return ToolOutput::text(if current.is_empty() {
                    "(no todo list yet)".to_string()
                } else {
                    current
                });
            };
            let mut items = Vec::with_capacity(todos.len());
            for item in todos {
                let status = match item.status.as_str() {
                    "pending" => Status::Pending,
                    "in_progress" => Status::InProgress,
                    "completed" => Status::Completed,
                    other => {
                        return ToolOutput::error(format!(
                            "invalid todo status {other:?}: use pending, in_progress or completed"
                        ));
                    }
                };
                items.push(TodoItem {
                    content: item.content,
                    status,
                });
            }
            let updated = TodoList { items };
            if let Err(error) = updated.validate() {
                return ToolOutput::error(error);
            }
            let rendered = updated.checklist();
            ToolOutput::text(if rendered.is_empty() {
                "(todo list cleared)".into()
            } else {
                rendered
            })
            .with_todo_state(list, updated)
        })
    }
}
