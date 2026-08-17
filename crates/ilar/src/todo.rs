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
    todos: Vec<ItemInput>,
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
        "Write the full todo list for the current task. Replaces the previous \
         list. Exactly one item may be in_progress."
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
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {"type": "string"},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]}
                        },
                        "required": ["content", "status"]
                    }
                }
            },
            "required": ["todos"]
        })
    }

    fn run(&self, input: serde_json::Value, _ctx: ToolContext) -> ToolFuture {
        let list = self.list.clone();
        Box::pin(async move {
            let input: Input = match serde_json::from_value(input) {
                Ok(v) => v,
                Err(e) => return ToolOutput::error(format!("invalid input for todo: {e}")),
            };
            let mut items = Vec::with_capacity(input.todos.len());
            for item in input.todos {
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
            let rendered: Vec<String> = updated
                .items
                .iter()
                .map(|i| format!("{} {}", i.status.marker(), i.content))
                .collect();
            ToolOutput::text(if rendered.is_empty() {
                "(todo list cleared)".into()
            } else {
                rendered.join("\n")
            })
            .with_todo_state(list, updated)
        })
    }
}
