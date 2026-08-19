use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ilar::tools::WorkspaceScheduler;
use ilar::tools::executor::{ToolCall, execute_calls};
use ilar::tools::{Tool, ToolConcurrency, ToolContext, ToolFuture, ToolOutput, WorkspaceAccess};
use tokio_util::sync::CancellationToken;

/// Recording probe tool: sleeps, logs "start:<name>" / "end:<name>" with
/// timestamps into a shared log.
struct ProbeTool {
    name: &'static str,
    concurrency: ToolConcurrency,
    sleep: Duration,
    log: Arc<Mutex<Vec<(Instant, String)>>>,
}

struct ConcurrentMutator(ProbeTool);

impl Tool for ConcurrentMutator {
    fn name(&self) -> &'static str {
        self.0.name
    }
    fn description(&self) -> &'static str {
        "concurrency-safe workspace mutator"
    }
    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Concurrent
    }
    fn workspace_access(&self) -> WorkspaceAccess {
        WorkspaceAccess::Mutating
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn run(&self, input: serde_json::Value, ctx: ToolContext) -> ToolFuture {
        self.0.run(input, ctx)
    }
}

impl Tool for ProbeTool {
    fn name(&self) -> &'static str {
        self.name
    }
    fn description(&self) -> &'static str {
        "probe"
    }
    fn concurrency(&self) -> ToolConcurrency {
        self.concurrency
    }
    fn workspace_access(&self) -> WorkspaceAccess {
        match self.concurrency {
            ToolConcurrency::Concurrent => WorkspaceAccess::ReadOnly,
            ToolConcurrency::Barrier => WorkspaceAccess::Mutating,
        }
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn run(&self, _input: serde_json::Value, _ctx: ToolContext) -> ToolFuture {
        let log = self.log.clone();
        let name = self.name.to_string();
        let sleep = self.sleep;
        Box::pin(async move {
            log.lock()
                .unwrap()
                .push((Instant::now(), format!("start:{name}")));
            tokio::time::sleep(sleep).await;
            log.lock()
                .unwrap()
                .push((Instant::now(), format!("end:{name}")));
            ToolOutput::text(format!("done:{name}"))
        })
    }
}

type ProbeLog = Arc<Mutex<Vec<(Instant, String)>>>;
type ToolMap = HashMap<String, Arc<dyn Tool>>;

fn harness() -> (ToolMap, ProbeLog) {
    let log = Arc::new(Mutex::new(Vec::new()));
    (HashMap::new(), log)
}

fn add(
    tools: &mut HashMap<String, Arc<dyn Tool>>,
    log: &Arc<Mutex<Vec<(Instant, String)>>>,
    name: &'static str,
    concurrency: ToolConcurrency,
    sleep: Duration,
) {
    tools.insert(
        name.into(),
        Arc::new(ProbeTool {
            name,
            concurrency,
            sleep,
            log: log.clone(),
        }),
    );
}

fn call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        input: serde_json::json!({}),
    }
}

fn ctx() -> ToolContext {
    ToolContext::root(std::env::temp_dir())
}

fn span(log: &[(Instant, String)], tag: &str) -> (Instant, Instant) {
    let start = log
        .iter()
        .find(|(_, t)| t == &format!("start:{tag}"))
        .map(|(t, _)| *t)
        .expect("start logged");
    let end = log
        .iter()
        .find(|(_, t)| t == &format!("end:{tag}"))
        .map(|(t, _)| *t)
        .expect("end logged");
    (start, end)
}

#[tokio::test]
async fn three_readonly_tools_overlap() {
    let (mut tools, log) = harness();
    add(
        &mut tools,
        &log,
        "r1",
        ToolConcurrency::Concurrent,
        Duration::from_millis(120),
    );
    add(
        &mut tools,
        &log,
        "r2",
        ToolConcurrency::Concurrent,
        Duration::from_millis(120),
    );
    add(
        &mut tools,
        &log,
        "r3",
        ToolConcurrency::Concurrent,
        Duration::from_millis(120),
    );

    let start = Instant::now();
    let outcomes = execute_calls(
        vec![call("1", "r1"), call("2", "r2"), call("3", "r3")],
        |n| tools.get(n).cloned(),
        ctx(),
        CancellationToken::new(),
    )
    .await;
    let elapsed = start.elapsed();

    assert_eq!(outcomes.len(), 3);
    assert!(outcomes.iter().all(|o| !o.output.is_error));
    // Deterministic overlap proof from the event log: every start happens
    // before any end (no timing dependence).
    let log = log.lock().unwrap();
    let last_start = log
        .iter()
        .filter(|(_, t)| t.starts_with("start:"))
        .map(|(t, _)| *t)
        .max()
        .expect("starts logged");
    let first_end = log
        .iter()
        .filter(|(_, t)| t.starts_with("end:"))
        .map(|(t, _)| *t)
        .min()
        .expect("ends logged");
    assert!(
        last_start < first_end,
        "no overlap: last start {last_start:?} >= first end {first_end:?}"
    );
    drop(log);
    // Wall-clock backup: 3x120ms serial would be 360ms; concurrent budget
    // is generous to survive a loaded machine.
    assert!(
        elapsed < Duration::from_millis(320),
        "execution looked serial: {elapsed:?}"
    );
}

#[tokio::test]
async fn concurrency_safe_workspace_mutators_are_serialized() {
    let (mut tools, log) = harness();
    for name in ["task_one", "task_two"] {
        tools.insert(
            name.into(),
            Arc::new(ConcurrentMutator(ProbeTool {
                name,
                concurrency: ToolConcurrency::Concurrent,
                sleep: Duration::from_millis(80),
                log: log.clone(),
            })),
        );
    }
    let outcomes = execute_calls(
        vec![call("1", "task_one"), call("2", "task_two")],
        |name| tools.get(name).cloned(),
        ctx(),
        CancellationToken::new(),
    )
    .await;
    assert!(outcomes.iter().all(|outcome| !outcome.output.is_error));
    let log = log.lock().unwrap();
    let (first_start, first_end) = span(&log, "task_one");
    let (second_start, second_end) = span(&log, "task_two");
    assert!(
        first_end <= second_start || second_end <= first_start,
        "workspace mutations overlapped"
    );
}

#[tokio::test]
async fn inherited_lease_from_another_scheduler_is_rejected() {
    let (mut tools, log) = harness();
    add(
        &mut tools,
        &log,
        "read",
        ToolConcurrency::Concurrent,
        Duration::from_millis(1),
    );
    let issuer = WorkspaceScheduler::new();
    let mut context = ctx();
    context.workspace_lease = Some(issuer.acquire_lease(WorkspaceAccess::ReadOnly).await);

    let outcomes = execute_calls(
        vec![call("1", "read")],
        |name| tools.get(name).cloned(),
        context,
        CancellationToken::new(),
    )
    .await;

    assert!(outcomes[0].output.is_error);
    assert!(log.lock().unwrap().is_empty());
}

#[tokio::test]
async fn mutating_tool_never_overlaps() {
    let (mut tools, log) = harness();
    add(
        &mut tools,
        &log,
        "slow_read",
        ToolConcurrency::Concurrent,
        Duration::from_millis(150),
    );
    add(
        &mut tools,
        &log,
        "edit",
        ToolConcurrency::Barrier,
        Duration::from_millis(60),
    );
    add(
        &mut tools,
        &log,
        "after_read",
        ToolConcurrency::Concurrent,
        Duration::from_millis(30),
    );

    let outcomes = execute_calls(
        vec![
            call("1", "slow_read"),
            call("2", "edit"),
            call("3", "after_read"),
        ],
        |n| tools.get(n).cloned(),
        ctx(),
        CancellationToken::new(),
    )
    .await;
    assert_eq!(outcomes.len(), 3);

    let log = log.lock().unwrap();
    let (_read_start, read_end) = span(&log, "slow_read");
    let (edit_start, edit_end) = span(&log, "edit");
    let (after_start, _after_end) = span(&log, "after_read");
    // edit runs strictly after the read-only tool ahead of it.
    assert!(
        edit_start >= read_end,
        "edit started before prior read finished"
    );
    // the read-only tool behind the edit waits for it.
    assert!(
        after_start >= edit_end,
        "read behind edit overlapped the edit"
    );
    drop(log);
}

#[tokio::test]
async fn results_in_call_order_despite_completion_order() {
    let (mut tools, _log) = harness();
    add(
        &mut tools,
        &_log,
        "slow",
        ToolConcurrency::Concurrent,
        Duration::from_millis(150),
    );
    add(
        &mut tools,
        &_log,
        "fast",
        ToolConcurrency::Concurrent,
        Duration::from_millis(5),
    );

    let outcomes = execute_calls(
        vec![call("a", "slow"), call("b", "fast")],
        |n| tools.get(n).cloned(),
        ctx(),
        CancellationToken::new(),
    )
    .await;

    assert_eq!(outcomes[0].id, "a");
    assert_eq!(outcomes[0].output.content, "done:slow");
    assert_eq!(outcomes[1].id, "b");
    assert_eq!(outcomes[1].output.content, "done:fast");
}

#[tokio::test]
async fn mutating_runs_alone_even_between_readonly() {
    // read(read-only) queued AFTER a mutating tool that is running must
    // not start until the mutating tool finishes.
    let (mut tools, log) = harness();
    add(
        &mut tools,
        &log,
        "bash_long",
        ToolConcurrency::Barrier,
        Duration::from_millis(150),
    );
    add(
        &mut tools,
        &log,
        "quick_read",
        ToolConcurrency::Concurrent,
        Duration::from_millis(10),
    );

    let outcomes = execute_calls(
        vec![call("1", "bash_long"), call("2", "quick_read")],
        |n| tools.get(n).cloned(),
        ctx(),
        CancellationToken::new(),
    )
    .await;
    assert_eq!(outcomes.len(), 2);

    let log = log.lock().unwrap();
    let (bash_start, bash_end) = span(&log, "bash_long");
    let (read_start, _) = span(&log, "quick_read");
    assert!(
        bash_start < read_start,
        "read overlapped running mutating tool"
    );
    assert!(
        read_start >= bash_end,
        "read started before mutating tool finished"
    );
    drop(log);
}

#[tokio::test]
async fn cancellation_stops_running_and_pending() {
    // Mutating tool runs first; the read-only tool behind it stays PENDING
    // (barrier) until cancel fires.
    let (mut tools, log) = harness();
    add(
        &mut tools,
        &log,
        "bash_long",
        ToolConcurrency::Barrier,
        Duration::from_secs(10),
    );
    add(
        &mut tools,
        &log,
        "never_read",
        ToolConcurrency::Concurrent,
        Duration::from_millis(5),
    );

    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel_clone.cancel();
    });

    let start = Instant::now();
    let outcomes = execute_calls(
        vec![call("1", "bash_long"), call("2", "never_read")],
        |n| tools.get(n).cloned(),
        ctx(),
        cancel,
    )
    .await;

    assert!(
        start.elapsed() < Duration::from_secs(3),
        "cancellation did not stop execution"
    );
    assert_eq!(outcomes.len(), 2);
    // Pin the id/name mapping through the hole-filling path.
    assert_eq!(outcomes[0].id, "1");
    assert_eq!(outcomes[0].name, "bash_long");
    assert!(outcomes[0].cancelled, "running tool not marked cancelled");
    assert!(outcomes[0].output.is_error);
    assert_eq!(outcomes[1].id, "2");
    assert_eq!(outcomes[1].name, "never_read");
    assert!(outcomes[1].cancelled, "pending tool not marked cancelled");
    // The pending tool must never have started.
    let log = log.lock().unwrap();
    assert!(
        !log.iter().any(|(_, t)| t == "start:never_read"),
        "pending tool started despite cancellation"
    );
}

#[tokio::test]
async fn unknown_tool_is_error_not_crash() {
    let (tools, _log) = harness();
    let outcomes = execute_calls(
        vec![call("1", "does_not_exist")],
        |n| tools.get(n).cloned(),
        ctx(),
        CancellationToken::new(),
    )
    .await;
    assert_eq!(outcomes.len(), 1);
    assert!(outcomes[0].output.is_error);
    assert!(!outcomes[0].cancelled);
    assert!(outcomes[0].output.content.contains("does_not_exist"));
}

#[tokio::test]
async fn pre_cancelled_token_starts_nothing() {
    let (mut tools, log) = harness();
    add(
        &mut tools,
        &log,
        "r1",
        ToolConcurrency::Concurrent,
        Duration::from_millis(5),
    );
    add(
        &mut tools,
        &log,
        "m1",
        ToolConcurrency::Barrier,
        Duration::from_millis(5),
    );

    let cancel = CancellationToken::new();
    cancel.cancel();
    let outcomes = execute_calls(
        vec![call("1", "r1"), call("2", "m1")],
        |n| tools.get(n).cloned(),
        ctx(),
        cancel,
    )
    .await;

    assert_eq!(outcomes.len(), 2);
    assert!(outcomes.iter().all(|o| o.cancelled));
    let log = log.lock().unwrap();
    assert!(
        log.is_empty(),
        "tools started despite pre-cancelled token: {log:?}"
    );
}

#[tokio::test]
async fn unknown_tool_mid_queue_keeps_index_alignment() {
    let (mut tools, log) = harness();
    add(
        &mut tools,
        &log,
        "r1",
        ToolConcurrency::Concurrent,
        Duration::from_millis(5),
    );
    add(
        &mut tools,
        &log,
        "r2",
        ToolConcurrency::Concurrent,
        Duration::from_millis(5),
    );

    let outcomes = execute_calls(
        vec![call("a", "r1"), call("b", "ghost"), call("c", "r2")],
        |n| tools.get(n).cloned(),
        ctx(),
        CancellationToken::new(),
    )
    .await;

    assert_eq!(outcomes.len(), 3);
    assert_eq!(outcomes[0].id, "a");
    assert_eq!(outcomes[0].output.content, "done:r1");
    assert_eq!(outcomes[1].id, "b");
    assert!(outcomes[1].output.is_error);
    assert!(outcomes[1].output.content.contains("ghost"));
    assert_eq!(outcomes[2].id, "c");
    assert_eq!(outcomes[2].output.content, "done:r2");
}

#[tokio::test]
async fn empty_calls_returns_empty() {
    let (tools, _log) = harness();
    let outcomes = execute_calls(
        Vec::new(),
        |n| tools.get(n).cloned(),
        ctx(),
        CancellationToken::new(),
    )
    .await;
    assert!(outcomes.is_empty());
}

/// The real mutating tools go through the executor's workspace protocol.
/// A tool whose `manages_workspace_access` / `accepts_executor_workspace_lease`
/// flags disagree takes the executor's *permit* branch and then awaits a
/// *lease* on the same workspace lock, deadlocking forever. Calling
/// `tool.run()` directly (as the tool tests do) cannot catch that, so
/// drive them through `execute_calls` here.
#[tokio::test]
async fn real_mutating_tools_complete_through_the_executor() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "original\n").unwrap();
    let registry = ilar::tools::ToolRegistry::builtin();
    let context = ToolContext::root(dir.path().to_path_buf());

    let outcomes = tokio::time::timeout(
        Duration::from_secs(10),
        execute_calls(
            vec![
                ToolCall {
                    id: "edit-1".into(),
                    name: "edit".into(),
                    input: serde_json::json!({
                        "path": "a.txt",
                        "old_string": "original",
                        "new_string": "edited"
                    }),
                },
                ToolCall {
                    id: "write-1".into(),
                    name: "write".into(),
                    input: serde_json::json!({"path": "b.txt", "content": "new"}),
                },
            ],
            |name| registry.get(name),
            context,
            CancellationToken::new(),
        ),
    )
    .await
    .expect("mutating tools deadlocked in the executor workspace protocol");

    for outcome in &outcomes {
        assert!(!outcome.output.is_error, "{}", outcome.output.content);
    }
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "edited\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("b.txt")).unwrap(),
        "new"
    );
}
