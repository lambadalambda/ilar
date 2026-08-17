use std::sync::{Arc, Mutex};

use ilar::session::SessionState;
use ilar::tools::executor::{ToolCall, execute_calls};
use ilar::tools::{ToolConcurrency, ToolContext, ToolRegistry, WorkspaceAccess};
use tokio_util::sync::CancellationToken;

fn registry() -> (ToolRegistry, Arc<Mutex<ilar::todo::TodoList>>) {
    let todos = Arc::new(Mutex::new(ilar::todo::TodoList::default()));
    (
        ToolRegistry::builtin().with_todos(todos.clone()).unwrap(),
        todos,
    )
}

async fn run(reg: &ToolRegistry, input: serde_json::Value) -> ilar::tools::ToolOutput {
    reg.get("todo")
        .expect("todo tool present")
        .run(input, ToolContext::root(std::env::temp_dir()))
        .await
}

#[tokio::test]
async fn todo_write_creates_items() {
    let (reg, _todos) = registry();
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
    let SessionState::TodoList { list: persisted } = out.session_state().expect("todo snapshot");
    assert_eq!(persisted.items.len(), 3);
    assert_eq!(persisted.items[1].status, ilar::todo::Status::InProgress);
}

#[tokio::test]
async fn todo_write_replaces_whole_list() {
    let (reg, _todos) = registry();
    let first = run(
        &reg,
        serde_json::json!({"todos": [{"content": "a", "status": "pending"}, {"content": "b", "status": "pending"}]}),
    )
    .await;
    let second = run(
        &reg,
        serde_json::json!({"todos": [{"content": "only this", "status": "in_progress"}]}),
    )
    .await;
    let SessionState::TodoList { list: first } = first.session_state().unwrap();
    let SessionState::TodoList { list: second } = second.session_state().unwrap();
    assert_eq!(first.items.len(), 2);
    assert_eq!(second.items.len(), 1);
    assert_eq!(second.items[0].content, "only this");
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
    assert_eq!(out.session_state(), None);
    assert!(
        out.content.to_lowercase().contains("one"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn todo_empty_list_is_allowed() {
    let (reg, todos) = registry();
    let output = run(&reg, serde_json::json!({"todos": []})).await;
    assert!(todos.lock().unwrap().items.is_empty());
    assert_eq!(
        output.session_state(),
        Some(&SessionState::TodoList {
            list: ilar::todo::TodoList::default(),
        })
    );
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
fn todo_tool_is_an_ordered_barrier_without_workspace_access() {
    let (reg, _) = registry();
    let tool = reg.get("todo").unwrap();
    assert_eq!(tool.concurrency(), ToolConcurrency::Barrier);
    assert_eq!(tool.workspace_access(), WorkspaceAccess::None);
}

#[tokio::test]
async fn todo_replacements_apply_in_provider_call_order() {
    let (reg, todos) = registry();
    let calls = ["first", "second"]
        .into_iter()
        .enumerate()
        .map(|(index, content)| ToolCall {
            id: index.to_string(),
            name: "todo".into(),
            input: serde_json::json!({"todos": [{"content": content, "status": "pending"}]}),
        });

    let outcomes = execute_calls(
        calls.collect(),
        |name| reg.get(name),
        ToolContext::root(std::env::temp_dir()),
        CancellationToken::new(),
    )
    .await;

    assert!(outcomes.iter().all(|outcome| !outcome.output.is_error));
    let snapshots = outcomes
        .iter()
        .map(|outcome| match outcome.output.session_state().unwrap() {
            SessionState::TodoList { list } => list.items[0].content.as_str(),
        })
        .collect::<Vec<_>>();
    assert_eq!(snapshots, ["first", "second"]);
    assert!(todos.lock().unwrap().items.is_empty());
}
