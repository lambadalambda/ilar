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
            "invalid service name",
        ),
        (serde_json::json!({"action": "logs"}), "requires name"),
        (
            serde_json::json!({"action": "stop", "name": "ghost"}),
            "no service named",
        ),
        (
            serde_json::json!({"action": "restart"}),
            "unknown service action",
        ),
    ] {
        let output = run(&tool, input).await;
        assert!(output.is_error);
        assert!(output.content.contains(needle), "{}", output.content);
    }
}
