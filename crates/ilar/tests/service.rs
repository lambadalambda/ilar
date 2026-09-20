use ilar::tools::service::{ServiceManager, ServiceTool};
use ilar::tools::{Tool, ToolContext};

fn ctx() -> ToolContext {
    ToolContext::root(std::env::temp_dir())
}

async fn run(tool: &ServiceTool, input: serde_json::Value) -> ilar::tools::ToolOutput {
    tool.run(input, ctx()).await
}

fn pid_alive(pid: u32) -> bool {
    // SAFETY: signal 0 probes liveness without sending anything.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

fn pid_from(start_output: &str) -> u32 {
    start_output
        .split("(pid ")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .and_then(|pid| pid.parse().ok())
        .expect("pid in start output")
}

#[tokio::test]
async fn service_round_trip_start_status_logs_stop() {
    let manager = ServiceManager::new();
    let tool = ServiceTool::new(manager.clone());

    let started = run(
        &tool,
        serde_json::json!({"action": "start", "name": "web", "command": "echo booted; sleep 30"}),
    )
    .await;
    assert!(!started.is_error, "{}", started.content);
    let pid = pid_from(&started.content);
    assert!(pid_alive(pid));
    assert_eq!(manager.running_count(), 1);

    // Duplicate start while running is refused.
    let duplicate = run(
        &tool,
        serde_json::json!({"action": "start", "name": "web", "command": "true"}),
    )
    .await;
    assert!(duplicate.is_error, "{}", duplicate.content);
    assert!(duplicate.content.contains("already running"));

    // Logs capture output after a moment.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let logs = run(&tool, serde_json::json!({"action": "logs", "name": "web"})).await;
    assert!(!logs.is_error);
    assert!(logs.content.contains("booted"), "{}", logs.content);
    assert!(logs.content.contains("running"), "{}", logs.content);

    let status = run(&tool, serde_json::json!({"action": "status"})).await;
    assert!(
        status.content.contains("web: running"),
        "{}",
        status.content
    );

    let stopped = run(&tool, serde_json::json!({"action": "stop", "name": "web"})).await;
    assert!(!stopped.is_error, "{}", stopped.content);
    assert!(!pid_alive(pid), "process must be dead after stop");
    assert_eq!(manager.running_count(), 0);

    // Restarting a stopped name is allowed.
    let restarted = run(
        &tool,
        serde_json::json!({"action": "start", "name": "web", "command": "sleep 30"}),
    )
    .await;
    assert!(!restarted.is_error, "{}", restarted.content);
    manager.stop_all();
}

#[tokio::test]
async fn exited_services_report_status_and_manager_drop_kills() {
    let manager = ServiceManager::new();
    let tool = ServiceTool::new(manager.clone());

    let exited = run(
        &tool,
        serde_json::json!({"action": "start", "name": "oneshot", "command": "exit 3"}),
    )
    .await;
    assert!(!exited.is_error, "{}", exited.content);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let status = run(
        &tool,
        serde_json::json!({"action": "status", "name": "oneshot"}),
    )
    .await;
    assert!(
        status.content.contains("stopped (exit 3)"),
        "{}",
        status.content
    );

    // Drop kills survivors — including grandchildren in the group.
    let survivor = run(
        &tool,
        serde_json::json!({"action": "start", "name": "daemonish", "command": "sleep 30 & sleep 30"}),
    )
    .await;
    let pid = pid_from(&survivor.content);
    assert!(pid_alive(pid));
    drop(tool);
    drop(manager);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(!pid_alive(pid), "manager drop must kill the service");
}

/// An exited service used to hold its whole 256 KiB capture for the
/// life of the process. The tail is kept — the reason it stopped is at
/// the bottom — and `log` still says the rest was dropped.
#[tokio::test]
async fn an_exited_service_keeps_only_the_tail_of_its_output() {
    let manager = ServiceManager::new();
    let tool = ServiceTool::new(manager.clone());

    // ~400 KiB of numbered lines, then a distinctive last word.
    let started = run(
        &tool,
        serde_json::json!({
            "action": "start",
            "name": "noisy",
            "command": "for i in $(seq 1 20000); do echo \"line $i padding padding padding\"; done; echo THE-LAST-WORD; exit 1",
        }),
    )
    .await;
    assert!(!started.is_error, "{}", started.content);
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;

    let status = run(
        &tool,
        serde_json::json!({"action": "status", "name": "noisy"}),
    )
    .await;
    assert!(status.content.contains("stopped"), "{}", status.content);

    let log = run(
        &tool,
        serde_json::json!({"action": "logs", "name": "noisy", "lines": 500}),
    )
    .await;
    // What was kept is the end, which is where anything went wrong.
    assert!(log.content.contains("THE-LAST-WORD"), "{}", log.content);
    // What was dropped is said, not silently missing.
    assert!(
        log.content.contains("earlier output dropped"),
        "{}",
        log.content
    );
    // And the head really is gone: the first lines cannot fit in the
    // tail that is kept.
    assert!(!log.content.contains("line 1 padding"), "{}", log.content);
}

#[tokio::test]
async fn service_input_validation() {
    let manager = ServiceManager::new();
    let tool = ServiceTool::new(manager.clone());
    for (input, needle) in [
        (
            serde_json::json!({"action": "start", "name": "web"}),
            "requires name and command",
        ),
        (
            serde_json::json!({"action": "start", "name": "no spaces", "command": "true"}),
            "service: invalid name",
        ),
        (serde_json::json!({"action": "logs"}), "requires name"),
        (
            serde_json::json!({"action": "stop", "name": "ghost"}),
            "no service named",
        ),
        (
            serde_json::json!({"action": "restart"}),
            "service: unknown action",
        ),
    ] {
        let output = run(&tool, input).await;
        assert!(output.is_error);
        assert!(output.content.contains(needle), "{}", output.content);
    }
}

/// A service started with a secret has it in its environment, and its
/// logs come back without the value.
#[tokio::test]
async fn service_start_takes_secrets_and_its_logs_hide_them() {
    let dir = tempfile::tempdir().unwrap();
    let store = ilar::secrets::SecretStore::open(dir.path());
    store.set("SVC_TOKEN", "", "svc-secret-value").unwrap();
    store.grant_always("SVC_TOKEN", "service").unwrap();
    let ctx = ctx().with_secrets(ilar::secrets::Secrets::new(store));
    let manager = ServiceManager::new();
    let tool = ServiceTool::new(manager.clone());

    let started = tool
        .run(
            serde_json::json!({"action": "start", "name": "sec", "command": "echo token=$SVC_TOKEN; sleep 30", "secrets": ["SVC_TOKEN"]}),
            ctx.clone(),
        )
        .await;
    assert!(!started.is_error, "{}", started.content);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let logs = tool
        .run(
            serde_json::json!({"action": "logs", "name": "sec"}),
            ctx.clone(),
        )
        .await;
    assert!(
        logs.content.contains("token=<secret:SVC_TOKEN>"),
        "{}",
        logs.content
    );
    assert!(!logs.content.contains("svc-secret-value"));
    let _ = tool
        .run(serde_json::json!({"action": "stop", "name": "sec"}), ctx)
        .await;

    // Unknown to a context without a store: refused before anything runs.
    let refused = run(
        &tool,
        serde_json::json!({"action": "start", "name": "bare", "command": "sleep 30", "secrets": ["SVC_TOKEN"]}),
    )
    .await;
    assert!(refused.is_error);
    assert_eq!(manager.running_count(), 0, "{}", refused.content);
}
