//! Session-scoped todo list tool — single `todo` write-tool rendering a
//! checklist (Claude Code todowrite style). ReadOnly for scheduling:
//! concurrent writes are last-write-wins on an Arc-shared list.

use std::sync::{Arc, Mutex};

use serde::Deserialize;

use crate::tools::{Tool, ToolContext, ToolFuture, ToolKind, ToolOutput};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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

#[derive(Debug, Clone, Default)]
pub struct TodoList {
    pub items: Vec<TodoItem>,
}

#[derive(Debug, Clone)]
pub struct TodoItem {
    pub content: String,
    pub status: Status,
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

    fn kind(&self) -> ToolKind {
        ToolKind::ReadOnly
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
            let mut in_progress = 0;
            for item in input.todos {
                let status = match item.status.as_str() {
                    "pending" => Status::Pending,
                    "in_progress" => {
                        in_progress += 1;
                        Status::InProgress
                    }
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
            if in_progress > 1 {
                return ToolOutput::error("at most one todo item may be in_progress at a time");
            }
            let mut list = list.lock().unwrap();
            *list = TodoList { items };
            let rendered: Vec<String> = list
                .items
                .iter()
                .map(|i| format!("{} {}", i.status.marker(), i.content))
                .collect();
            drop(list);
            ToolOutput::text(if rendered.is_empty() {
                "(todo list cleared)".into()
            } else {
                rendered.join("\n")
            })
        })
    }
}
