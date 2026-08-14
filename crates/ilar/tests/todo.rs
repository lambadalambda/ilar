use std::sync::{Arc, Mutex};

use ilar::tools::{ToolContext, ToolKind, ToolRegistry};

fn registry() -> (ToolRegistry, Arc<Mutex<ilar::todo::TodoList>>) {
    let todos = Arc::new(Mutex::new(ilar::todo::TodoList::default()));
    (ToolRegistry::builtin().with_todos(todos.clone()), todos)
}

async fn run(reg: &ToolRegistry, input: serde_json::Value) -> ilar::tools::ToolOutput {
    reg.get("todo")
        .expect("todo tool present")
        .run(input, ToolContext::root(std::env::temp_dir()))
        .await
}

#[tokio::test]
async fn todo_write_creates_items() {
    let (reg, todos) = registry();
    let out = run(
        &reg,
        serde_json::json!({"todos": [
            {"content": "first thing", "status": "completed"},
            {"content": "second thing", "status": "in_progress"},
            {"content": "third thing", "status": "pending"}
        ]}),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    let list = todos.lock().unwrap();
    assert_eq!(list.items.len(), 3);
    assert_eq!(list.items[0].status, ilar::todo::Status::Completed);
    assert_eq!(list.items[1].status, ilar::todo::Status::InProgress);
}

#[tokio::test]
async fn todo_write_replaces_whole_list() {
    let (reg, todos) = registry();
    run(
        &reg,
        serde_json::json!({"todos": [{"content": "a", "status": "pending"}, {"content": "b", "status": "pending"}]}),
    )
    .await;
    run(
        &reg,
        serde_json::json!({"todos": [{"content": "only this", "status": "in_progress"}]}),
    )
    .await;
    let list = todos.lock().unwrap();
    assert_eq!(list.items.len(), 1);
    assert_eq!(list.items[0].content, "only this");
}

#[tokio::test]
async fn todo_write_rejects_two_in_progress() {
    let (reg, _todos) = registry();
    let out = run(
        &reg,
        serde_json::json!({"todos": [
            {"content": "a", "status": "in_progress"},
            {"content": "b", "status": "in_progress"}
        ]}),
    )
    .await;
    assert!(out.is_error);
    assert!(
        out.content.to_lowercase().contains("one"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn todo_empty_list_is_allowed() {
    let (reg, todos) = registry();
    run(&reg, serde_json::json!({"todos": []})).await;
    assert!(todos.lock().unwrap().items.is_empty());
}

#[tokio::test]
async fn todo_output_renders_checklist() {
    let (reg, _todos) = registry();
    let out = run(
        &reg,
        serde_json::json!({"todos": [
            {"content": "done thing", "status": "completed"},
            {"content": "active thing", "status": "in_progress"},
            {"content": "later thing", "status": "pending"}
        ]}),
    )
    .await;
    assert!(out.content.contains("[x] done thing"), "{}", out.content);
    assert!(
        out.content.contains("[*] active thing")
            || out.content.contains("[>] active thing")
            || out.content.contains("active thing"),
        "{}",
        out.content
    );
    assert!(out.content.contains("[ ] later thing"), "{}", out.content);
}

#[test]
fn todo_tool_is_read_only_for_scheduling() {
    let (reg, _) = registry();
    assert_eq!(reg.get("todo").unwrap().kind(), ToolKind::ReadOnly);
}
