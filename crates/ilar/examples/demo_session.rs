//! Fabricate a demo state directory for documentation screenshots.
//!
//! ```sh
//! cargo run -p ilar --example demo_session -- /tmp/ilar-demo-state
//! ILAR_STATE_DIR=/tmp/ilar-demo-state ilar --continue
//! ```
//!
//! Everything goes through the real session API, so the fixtures stay
//! valid whenever the format moves.

use chrono::Utc;
use ilar::session::{
    ContentBlock, SessionEvent, SessionMeta, SessionState, SessionStore, Usage, new_id,
};
use ilar::todo::{Status, TodoItem, TodoList};

fn meta(model: &str) -> SessionMeta {
    SessionMeta {
        session_id: new_id(),
        parent_id: None,
        agent: "build".into(),
        model: model.into(),
        workspace: None,
    }
}

fn user(text: &str) -> SessionEvent {
    SessionEvent::UserMessage {
        id: new_id(),
        text: text.into(),
        ts: Utc::now(),
    }
}

fn topic(text: &str) -> SessionEvent {
    SessionEvent::Topic {
        id: new_id(),
        text: text.into(),
        ts: Utc::now(),
    }
}

fn assistant(content: Vec<ContentBlock>, usage: Usage) -> SessionEvent {
    SessionEvent::AssistantMessage {
        id: new_id(),
        model: "openai/gpt-5.6-sol".into(),
        content,
        usage,
        stop_reason: "end_turn".into(),
        ts: Utc::now(),
    }
}

fn thinking(text: &str) -> ContentBlock {
    // ReasoningSummary is what the restored transcript renders as a
    // "Thought:" row.
    ContentBlock::ReasoningSummary {
        text: text.into(),
        completed: true,
    }
}

fn call(id: &str, name: &str, input: serde_json::Value) -> ContentBlock {
    ContentBlock::ToolCall {
        id: id.into(),
        name: name.into(),
        input,
        item_id: None,
    }
}

fn result(id: &str, content: &str) -> SessionEvent {
    SessionEvent::ToolResult {
        id: new_id(),
        tool_use_id: id.into(),
        content: content.into(),
        is_error: false,
        child_session_id: None,
        state: None,
        ts: Utc::now(),
    }
}

fn todo_result(id: &str, items: Vec<(&str, Status)>) -> SessionEvent {
    let list = TodoList {
        items: items
            .into_iter()
            .map(|(content, status)| TodoItem {
                content: content.into(),
                status,
            })
            .collect(),
    };
    SessionEvent::ToolResult {
        id: new_id(),
        tool_use_id: id.into(),
        content: list.checklist(),
        is_error: false,
        child_session_id: None,
        state: Some(SessionState::TodoList { list }),
        ts: Utc::now(),
    }
}

fn usage(input: u64, output: u64) -> Usage {
    // Mostly cache-served input, like a healthy agentic session.
    Usage {
        input_tokens: input / 10,
        output_tokens: output,
        cache_read_input_tokens: input - input / 10,
        input_token_accounting: Some(ilar::session::InputTokenAccounting::ExcludesCached),
        ..Default::default()
    }
}

fn main() {
    let dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/ilar-demo-state".into());
    let store = SessionStore::new(std::path::PathBuf::from(&dir).join("sessions"));

    // Older sessions so the picker and search have company; several
    // share the word "timeout" so a search query fans out nicely.
    for (opening, name, middle) in [
        (
            "can you dig into the GM1 firmware image in fixtures/?",
            "GM1 firmware dig",
            "the AES key schedule table lives at offset 0x4f11b4, right after the update header",
        ),
        (
            "the nginx config has grown three conflicting proxy blocks, clean it up",
            "Nginx proxy cleanup",
            "collapsed the three location blocks into one upstream, and raised proxy_read_timeout to 90s for the export routes",
        ),
        (
            "our macos CI runners keep flaking on the websocket suite",
            "Flaky websocket CI",
            "the flake was a 5s handshake timeout racing runner cold-starts; bumped it to 30s and pinned the runner image",
        ),
        (
            "walk me through a dry run of the payments schema migration",
            "Payments migration dry-run",
            "the dry run passes once statement_timeout is disabled for the backfill transaction; plan is to run it in batches of 10k",
        ),
        (
            "why does the settings page take four seconds to load?",
            "Settings page latency",
            "three sequential fetches with no cache — batched them into one query and memoized the feature flags",
        ),
    ] {
        let mut session = store.create(meta("zai/glm-4.7")).unwrap();
        session.append(user(opening)).unwrap();
        session.append(topic(name)).unwrap();
        session
            .append(assistant(
                vec![ContentBlock::Text {
                    text: middle.into(),
                }],
                usage(900, 60),
            ))
            .unwrap();
    }

    // The hero session: a believable bug hunt.
    let mut session = store.create(meta("openai/gpt-5.6-sol")).unwrap();
    session
        .append(user(
            "requests to /export sometimes hang forever — find out why and fix it",
        ))
        .unwrap();
    session.append(topic("Export endpoint hang fix")).unwrap();

    session
        .append(assistant(
            vec![
                thinking("Scoping the hang: reading the export handler and its writer"),
                call(
                    "c1",
                    "grep",
                    serde_json::json!({"pattern": "/export", "path": "src"}),
                ),
                call("c2", "read", serde_json::json!({"path": "src/export.rs"})),
                call("c3", "read", serde_json::json!({"path": "src/writer.rs"})),
            ],
            usage(1_800, 140),
        ))
        .unwrap();
    session
        .append(result(
            "c1",
            "src/routes.rs:88: .route(\"/export\", get(export_all))",
        ))
        .unwrap();
    session
        .append(result(
            "c2",
            "pub async fn export_all(state: State) -> impl IntoResponse {\n    let (tx, rx) = mpsc::channel(0);\n    …",
        ))
        .unwrap();
    session
        .append(result(
            "c3",
            "fn drain(rx: Receiver<Row>) { while let Some(row) = rx.blocking_recv() { … } }",
        ))
        .unwrap();

    session
        .append(assistant(
            vec![
                thinking(
                    "Found it: a zero-capacity channel between the exporter and a \
                     blocking writer — when the client stalls, the writer never \
                     yields and the whole task deadlocks",
                ),
                call(
                    "c4",
                    "todo",
                    serde_json::json!({"todos": [
                        {"content": "Reproduce the hang with a stalling client", "status": "completed"},
                        {"content": "Bound the exporter/writer channel", "status": "completed"},
                        {"content": "Time out abandoned exports after 30s", "status": "in_progress"},
                        {"content": "Regression test with a slow reader", "status": "pending"},
                    ]}),
                ),
            ],
            usage(2_400, 210),
        ))
        .unwrap();
    session
        .append(todo_result(
            "c4",
            vec![
                (
                    "Reproduce the hang with a stalling client",
                    Status::Completed,
                ),
                ("Bound the exporter/writer channel", Status::Completed),
                ("Time out abandoned exports after 30s", Status::InProgress),
                ("Regression test with a slow reader", Status::Pending),
            ],
        ))
        .unwrap();

    session
        .append(assistant(
            vec![
                thinking("Delegating a survey of every other channel(0) in the tree"),
                call(
                    "c5",
                    "task",
                    serde_json::json!({
                        "subagent_type": "explore",
                        "description": "Audit other zero-capacity channels",
                        "prompt": "Find every mpsc::channel(0) in src/ and report which ones pair with blocking consumers."
                    }),
                ),
            ],
            usage(2_900, 120),
        ))
        .unwrap();
    session
        .append(result(
            "c5",
            "Two more: src/metrics.rs:41 (safe, consumer is async) and src/backup.rs:77 \
             (same deadlock shape as export — worth a follow-up).\ntask_id: 3f2a9c1e-demo",
        ))
        .unwrap();

    session
        .append(assistant(
            vec![
                thinking("Applying the fix and running the suite"),
                call("c6", "edit", serde_json::json!({"path": "src/export.rs"})),
                call(
                    "c7",
                    "bash",
                    serde_json::json!({"command": "cargo test -p kestrel export"}),
                ),
            ],
            usage(3_400, 260),
        ))
        .unwrap();
    session
        .append(result(
            "c6",
            "replaced channel(0) with channel(1024) + send timeout",
        ))
        .unwrap();
    session
        .append(result(
            "c7",
            "test export::streams_all_rows ... ok\ntest export::stalling_client_times_out ... ok\n\ntest result: ok. 14 passed; 0 failed",
        ))
        .unwrap();

    session
        .append(assistant(
            vec![ContentBlock::Text {
                text: "Found and fixed. The hang was a deadlock between the export \
                       handler and its writer:\n\n\
                       - `export_all` fed rows through an **unbounded rendezvous** — \
                       `mpsc::channel(0)` — so every send waited for the writer.\n\
                       - The writer drained it with `blocking_recv` on the same \
                       runtime; when a client stalled mid-download, the writer never \
                       yielded and the task deadlocked with the row producer.\n\n\
                       The fix bounds the channel and abandons stalled clients:\n\n\
                       ```rust\n\
                       let (tx, rx) = mpsc::channel(1024);\n\
                       tx.send_timeout(row, EXPORT_STALL_LIMIT).await?;\n\
                       ```\n\n\
                       All 14 export tests pass, including a new one that stalls the \
                       client on purpose. The explore agent found the same shape in \
                       `src/backup.rs:77` — left as a todo since it's outside this \
                       fix."
                    .into(),
            }],
            usage(3_900, 480),
        ))
        .unwrap();

    println!("demo state written to {dir}");
}
