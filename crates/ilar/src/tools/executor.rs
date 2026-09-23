//! Concurrency-barrier tool executor — see meta/issues/tool-executor-barrier.md.
//!
//! Concurrent tools may overlap within a provider step; a barrier tool runs
//! alone. Workspace read/write exclusion is enforced independently.
//! Execution is concurrent, results are returned in call order.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::stream::{FuturesUnordered, StreamExt};
use tokio_util::sync::CancellationToken;

use super::{
    Tool, ToolConcurrency, ToolContext, ToolOutput, ToolStartObserver, WorkspaceAccess,
    WorkspaceCoverage, WorkspaceWaitNotice,
};

/// One tool call from an assistant turn.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

/// Result of one call, positioned in the original call order.
#[derive(Debug, Clone)]
pub struct CallOutcome {
    pub id: String,
    pub name: String,
    pub output: ToolOutput,
    /// True when the call was aborted mid-run or never started.
    pub cancelled: bool,
}

struct Running {
    idx: usize,
    id: String,
    name: String,
    concurrency: ToolConcurrency,
    access: WorkspaceAccess,
}

/// What an unknown tool name is told: the names it could have used.
/// A bare "no such tool: x" is one the model can only answer by
/// guessing again.
fn unknown_tool_refusal(name: &str, known: &[&'static str]) -> String {
    if known.is_empty() {
        return format!("no such tool: {name}");
    }
    format!(
        "no such tool: {name}; this session has: {}",
        known.join(", ")
    )
}

/// What a mutating call behind a detached holder is told instead of
/// waiting: the step would hold until that job reports, and a message
/// from the person would wait with it.
fn workspace_held_refusal(name: &str) -> String {
    format!(
        "{name}: not run — this checkout is held by another job of this session until it ends: a \
         background task that may edit, a background bash, or a task the person resumed from \
         its own view. Do not retry at once: read, glob and grep still work, a background \
         job's completion reaches you as a notification, and the tasks tool shows what is \
         still running. If you need this call, end your turn and make it once the checkout is \
         free."
    )
}

/// Execute a turn's tool calls under the barrier discipline.
///
/// `resolve` maps tool names to implementations (usually the registry).
/// Unknown tools produce an error outcome without executing.
///
/// Cancellation: on cancel (or drop of the returned future), running tool
/// futures are dropped (cooperative cancellation; bash kills its child
/// via `kill_on_drop`), pending calls never start; both are marked
/// cancelled.
#[allow(clippy::type_complexity)] // resolver closure; inherent shape
pub async fn execute_calls<F>(
    calls: Vec<ToolCall>,
    resolve: F,
    ctx: ToolContext,
    cancel: CancellationToken,
) -> Vec<CallOutcome>
where
    F: Fn(&str) -> Option<Arc<dyn Tool>>,
{
    execute_calls_observed(
        calls,
        resolve,
        Vec::new(),
        ctx,
        cancel,
        |_, _| {},
        |_, _| {},
    )
    .await
}

#[allow(clippy::type_complexity)]
pub(crate) async fn execute_calls_observed<F, O, C>(
    calls: Vec<ToolCall>,
    resolve: F,
    // What this session actually has, for the refusal an unknown name
    // earns: "no such tool: x" alone left the model to guess again.
    known: Vec<&'static str>,
    ctx: ToolContext,
    cancel: CancellationToken,
    on_start: O,
    on_complete: C,
) -> Vec<CallOutcome>
where
    F: Fn(&str) -> Option<Arc<dyn Tool>>,
    O: Fn(String, String) + Clone + Send + 'static,
    C: Fn(String, String) + Clone + Send + 'static,
{
    let call_count = calls.len();
    let mut outcomes: Vec<Option<CallOutcome>> = calls.iter().map(|_| None).collect();
    let mut pending: VecDeque<ToolCall> = calls.into_iter().collect();
    let mut running_meta: Vec<Running> = Vec::new();
    // (idx, future) pairs wrapped into single futures — tuples of futures
    // don't implement Future.
    type RunningFuture = Pin<Box<dyn Future<Output = (usize, ToolOutput, bool)> + Send>>;
    let mut running: FuturesUnordered<RunningFuture> = FuturesUnordered::new();
    let mut next_idx = 0usize;

    let cancelled = loop {
        // A cancel that fired while we weren't polling (or raced a
        // completion) must not let a new scheduling pass start tools.
        if cancel.is_cancelled() {
            break true;
        }
        // Schedule from the front while the barrier allows.
        while let Some(call) = pending.front() {
            let Some(tool) = resolve(&call.name) else {
                // Unknown tool: immediate error, no execution.
                let call = pending.pop_front().unwrap();
                let idx = next_idx;
                next_idx += 1;
                outcomes[idx] = Some(CallOutcome {
                    name: call.name.clone(),
                    id: call.id.clone(),
                    output: ToolOutput::error(unknown_tool_refusal(&call.name, &known)),
                    cancelled: false,
                });
                continue;
            };
            let concurrency = tool.concurrency();
            let all_concurrent = running_meta
                .iter()
                .all(|running| running.concurrency == ToolConcurrency::Concurrent);
            let can_start = running_meta.is_empty()
                || (concurrency == ToolConcurrency::Concurrent && all_concurrent);
            if !can_start {
                break; // barrier holds the queue behind it
            }
            let call = pending.pop_front().unwrap();
            let idx = next_idx;
            next_idx += 1;
            // Asked here rather than in each file tool: a path withheld
            // from `read` is withheld from the `bash` that would cat it,
            // and one gate cannot be half-applied to a tool added later.
            if let Some(refusal) = ctx.withheld_refusal(&call.input) {
                outcomes[idx] = Some(CallOutcome {
                    output: ToolOutput::error(format!("{}: {refusal}", call.name)),
                    id: call.id,
                    name: call.name,
                    cancelled: false,
                });
                continue;
            }
            let background = tool.supports_background()
                && call
                    .input
                    .get("run_in_background")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true);
            if background && call_count != 1 {
                outcomes[idx] = Some(CallOutcome {
                    output: ToolOutput::error(format!(
                        "{}: a background call must be the only tool call in a provider step; \
                         send it alone, or run it in the foreground",
                        call.name
                    )),
                    id: call.id,
                    name: call.name,
                    cancelled: false,
                });
                continue;
            }
            let access = tool.workspace_access_for(&call.input);
            // The one holder worth waiting for is a mutator of this very
            // step, which finishes inside it. Anything else holding the
            // checkout is outside the step — a background task or bash —
            // and the model chose not to wait for that. No built-in
            // mutator reaches the sibling wait today (the ones that take
            // this branch are barriers); it is kept for a concurrent one.
            let waits_for_sibling = running_meta
                .iter()
                .any(|running| running.access == WorkspaceAccess::Mutating);
            running_meta.push(Running {
                idx,
                id: call.id.clone(),
                name: call.name.clone(),
                concurrency,
                access,
            });
            let manages_workspace_access = tool.manages_workspace_access();
            let accepts_executor_workspace_lease = tool.accepts_executor_workspace_lease();
            let mut call_ctx = ctx.clone();
            call_ctx.call_id = Some(call.id.clone());
            let secrets = call_ctx.secrets.clone();
            let input = call.input;
            let on_start = on_start.clone();
            let started_id = call.id.clone();
            let started_name = call.name.clone();
            // This call's own id, for the notes its asks left behind.
            let scrub_id = call.id.clone();
            running.push(Box::pin(async move {
                let started = Arc::new(AtomicBool::new(false));
                let observed_start = started.clone();
                let start: ToolStartObserver = Box::new(move || {
                    observed_start.store(true, Ordering::SeqCst);
                    on_start(started_id, started_name);
                });
                let output = if background {
                    tool.run_observed(input, call_ctx, start).await
                } else if manages_workspace_access && accepts_executor_workspace_lease {
                    match call_ctx.workspace_coverage(access) {
                        WorkspaceCoverage::Covered => {
                            tool.run_observed(input, call_ctx, start).await
                        }
                        WorkspaceCoverage::Absent => {
                            // Same rule, same notice as the plain-permit
                            // branch below: edit/write must not sit on a
                            // silent row while a sibling holds the
                            // checkout.
                            let lease = match call_ctx.workspace.try_acquire_lease(access) {
                                Some(lease) => Some(lease),
                                None if waits_for_sibling => {
                                    WorkspaceWaitNotice::announce(
                                        WorkspaceWaitNotice::from_context(&call_ctx).as_ref(),
                                    );
                                    Some(call_ctx.workspace.acquire_lease(access).await)
                                }
                                None => None,
                            };
                            match lease {
                                Some(lease) => {
                                    call_ctx.workspace_lease = Some(lease);
                                    tool.run_observed(input, call_ctx, start).await
                                }
                                None => ToolOutput::error(workspace_held_refusal(tool.name())),
                            }
                        }
                        WorkspaceCoverage::Incompatible => ToolOutput::error(format!(
                            "{}: workspace access is not covered by its inherited lease",
                            tool.name()
                        )),
                    }
                } else if manages_workspace_access || access == WorkspaceAccess::None {
                    tool.run_observed(input, call_ctx, start).await
                } else {
                    match call_ctx.workspace_coverage(access) {
                        WorkspaceCoverage::Covered => {
                            tool.run_observed(input, call_ctx, start).await
                        }
                        WorkspaceCoverage::Absent => {
                            // The only wait left is on a sibling writer;
                            // say so, or "queued" reads as a hang.
                            let permit = match call_ctx.workspace.try_acquire(access) {
                                Some(permit) => Some(permit),
                                None if waits_for_sibling => {
                                    WorkspaceWaitNotice::announce(
                                        WorkspaceWaitNotice::from_context(&call_ctx).as_ref(),
                                    );
                                    Some(call_ctx.workspace.acquire(access).await)
                                }
                                None => None,
                            };
                            match permit {
                                Some(_permit) => tool.run_observed(input, call_ctx, start).await,
                                None => ToolOutput::error(workspace_held_refusal(tool.name())),
                            }
                        }
                        WorkspaceCoverage::Incompatible => ToolOutput::error(format!(
                            "{}: workspace access is not covered by its inherited lease",
                            tool.name()
                        )),
                    }
                };
                // Every stored value, out of every result, whatever
                // the tool: a `read` of the store file, a `grep` that
                // crosses it, a shell that prints one. The one place
                // all results pass, so the one place to hold the line.
                let output = match &secrets {
                    Some(secrets) => output.scrubbed(secrets, Some(&scrub_id)),
                    None => output,
                };
                (idx, output, started.load(Ordering::SeqCst))
            }));
        }

        if running.is_empty() {
            break false;
        }

        tokio::select! {
            maybe = running.next() => {
                let Some((idx, output, started)) = maybe else { continue };
                if let Some(pos) = running_meta.iter().position(|r| r.idx == idx) {
                    let meta = running_meta.remove(pos);
                    if started {
                        on_complete(meta.id.clone(), meta.name.clone());
                    }
                    outcomes[idx] = Some(CallOutcome {
                        id: meta.id,
                        name: meta.name,
                        output,
                        cancelled: false,
                    });
                }
            }
            _ = cancel.cancelled() => {
                break true;
            }
        }
    };

    // Fill any holes (cancelled running calls, never-started pending).
    let cancelled_meta: Vec<(String, String)> = running_meta
        .iter()
        .map(|r| (r.id.clone(), r.name.clone()))
        .collect();
    let mut cancelled_iter = cancelled_meta.into_iter();
    let mut pending_iter = pending.into_iter();
    outcomes
        .into_iter()
        .enumerate()
        .map(|(idx, outcome)| {
            outcome.unwrap_or_else(|| {
                let (id, name) = if cancelled {
                    // First holes are running calls (they were popped from
                    // pending in start order), then never-started ones.
                    cancelled_iter
                        .next()
                        .or_else(|| pending_iter.next().map(|c| (c.id, c.name)))
                        .unwrap_or_default()
                } else {
                    unreachable!("no cancel, but outcome {idx} missing")
                };
                CallOutcome {
                    output: ToolOutput::error(if name.is_empty() {
                        "cancelled".to_string()
                    } else {
                        format!("{name}: cancelled")
                    }),
                    id,
                    name,
                    cancelled: true,
                }
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// "no such tool: x" alone leaves the model guessing a second name.
    #[test]
    fn an_unknown_tool_is_told_what_this_session_has() {
        let refusal = unknown_tool_refusal("edit_file", &["read", "edit", "write"]);
        assert_eq!(
            refusal,
            "no such tool: edit_file; this session has: read, edit, write"
        );
        // A caller that cannot list its tools still says what happened.
        assert_eq!(
            unknown_tool_refusal("edit_file", &[]),
            "no such tool: edit_file"
        );
    }

    struct GateTool {
        gate: Arc<tokio::sync::Notify>,
    }

    impl Tool for GateTool {
        fn name(&self) -> &'static str {
            "gate"
        }

        fn description(&self) -> &'static str {
            "waits for a test gate"
        }

        fn concurrency(&self) -> ToolConcurrency {
            ToolConcurrency::Barrier
        }

        fn workspace_access(&self) -> WorkspaceAccess {
            WorkspaceAccess::None
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        fn run(&self, _input: serde_json::Value, _ctx: ToolContext) -> super::super::ToolFuture {
            let gate = self.gate.clone();
            Box::pin(async move {
                gate.notified().await;
                ToolOutput::text("released")
            })
        }
    }

    struct ImmediateTool;

    impl Tool for ImmediateTool {
        fn name(&self) -> &'static str {
            "immediate"
        }

        fn description(&self) -> &'static str {
            "returns immediately"
        }

        fn concurrency(&self) -> ToolConcurrency {
            ToolConcurrency::Barrier
        }

        fn workspace_access(&self) -> WorkspaceAccess {
            WorkspaceAccess::None
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        fn run(&self, _input: serde_json::Value, _ctx: ToolContext) -> super::super::ToolFuture {
            Box::pin(async { ToolOutput::text("done") })
        }
    }

    struct RejectedBeforeStartTool;

    impl Tool for RejectedBeforeStartTool {
        fn name(&self) -> &'static str {
            "rejected"
        }

        fn description(&self) -> &'static str {
            "rejects before execution"
        }

        fn concurrency(&self) -> ToolConcurrency {
            ToolConcurrency::Barrier
        }

        fn workspace_access(&self) -> WorkspaceAccess {
            WorkspaceAccess::None
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        fn run(&self, _input: serde_json::Value, _ctx: ToolContext) -> super::super::ToolFuture {
            Box::pin(async { ToolOutput::error("rejected") })
        }

        fn run_observed(
            &self,
            _input: serde_json::Value,
            _ctx: ToolContext,
            _on_start: ToolStartObserver,
        ) -> super::super::ToolFuture {
            Box::pin(async { ToolOutput::error("rejected") })
        }
    }

    struct ManagedLeaseTool;

    impl Tool for ManagedLeaseTool {
        fn name(&self) -> &'static str {
            "managed"
        }

        fn description(&self) -> &'static str {
            "accepts an executor workspace lease"
        }

        fn concurrency(&self) -> ToolConcurrency {
            ToolConcurrency::Barrier
        }

        fn workspace_access(&self) -> WorkspaceAccess {
            WorkspaceAccess::Mutating
        }

        fn manages_workspace_access(&self) -> bool {
            true
        }

        fn accepts_executor_workspace_lease(&self) -> bool {
            true
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        fn run(&self, _input: serde_json::Value, ctx: ToolContext) -> super::super::ToolFuture {
            Box::pin(async move {
                assert_eq!(
                    ctx.workspace_coverage(WorkspaceAccess::Mutating),
                    WorkspaceCoverage::Covered
                );
                ToolOutput::text("done")
            })
        }
    }

    #[tokio::test]
    async fn start_observer_fires_when_each_barrier_tool_actually_starts() {
        let dir = tempfile::tempdir().unwrap();
        let gate = Arc::new(tokio::sync::Notify::new());
        let gate_tool: Arc<dyn Tool> = Arc::new(GateTool { gate: gate.clone() });
        let immediate_tool: Arc<dyn Tool> = Arc::new(ImmediateTool);
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let lifecycle = Arc::new(std::sync::Mutex::new(Vec::new()));
        let started_lifecycle = lifecycle.clone();
        let completed_lifecycle = lifecycle.clone();
        let execution = execute_calls_observed(
            vec![
                ToolCall {
                    id: "gate-1".into(),
                    name: "gate".into(),
                    input: serde_json::json!({}),
                },
                ToolCall {
                    id: "immediate-1".into(),
                    name: "immediate".into(),
                    input: serde_json::json!({}),
                },
            ],
            move |name| match name {
                "gate" => Some(gate_tool.clone()),
                "immediate" => Some(immediate_tool.clone()),
                _ => None,
            },
            vec!["gate", "immediate"],
            ToolContext::root(dir.path().to_path_buf()),
            CancellationToken::new(),
            move |id, _| {
                started_lifecycle
                    .lock()
                    .unwrap()
                    .push(format!("start:{id}"));
                let _ = started_tx.send(id);
            },
            move |id, _| {
                completed_lifecycle
                    .lock()
                    .unwrap()
                    .push(format!("complete:{id}"));
            },
        );
        let execution = tokio::spawn(execution);

        assert_eq!(started_rx.recv().await.as_deref(), Some("gate-1"));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), started_rx.recv())
                .await
                .is_err(),
            "queued barrier was reported as executing"
        );
        gate.notify_one();
        assert_eq!(started_rx.recv().await.as_deref(), Some("immediate-1"));
        assert!(
            execution
                .await
                .unwrap()
                .iter()
                .all(|outcome| !outcome.output.is_error)
        );
        assert_eq!(
            *lifecycle.lock().unwrap(),
            [
                "start:gate-1",
                "complete:gate-1",
                "start:immediate-1",
                "complete:immediate-1",
            ]
        );
    }

    /// A withheld path is withheld from every tool, named or not: the
    /// call is refused before the tool runs, whatever it was going to
    /// do with the path, and the refusal does not read the path back
    /// out into the chat that must not have it.
    #[tokio::test]
    async fn a_call_naming_a_withheld_path_is_refused_before_it_runs() {
        let dir = tempfile::tempdir().unwrap();
        // Canonical, as a tool context's own cwd is: on macOS a temp
        // directory is reached through a symlink, and the two spellings
        // of one directory must not read as two directories.
        let root = dir.path().canonicalize().unwrap();
        let memory = root.join("home").join("memory");
        let immediate: Arc<dyn Tool> = Arc::new(ImmediateTool);
        let outcomes = execute_calls(
            vec![
                ToolCall {
                    id: "read-1".into(),
                    name: "immediate".into(),
                    input: serde_json::json!({
                        "path": memory.join("USER.md").to_str().unwrap(),
                    }),
                },
                ToolCall {
                    id: "bash-1".into(),
                    name: "immediate".into(),
                    input: serde_json::json!({
                        "command": format!("cat {}/USER.md", memory.display()),
                    }),
                },
                // The spelling a model reaches for once its own prompt
                // has told it where home is: relative, from the
                // workspace next door.
                ToolCall {
                    id: "relative-1".into(),
                    name: "immediate".into(),
                    input: serde_json::json!({"path": "home/memory/../memory/USER.md"}),
                },
                ToolCall {
                    id: "elsewhere-1".into(),
                    name: "immediate".into(),
                    input: serde_json::json!({"command": "ls"}),
                },
            ],
            move |_| Some(immediate.clone()),
            ToolContext::root(root.clone()).with_withheld(Arc::from(vec![memory.clone()])),
            CancellationToken::new(),
        )
        .await;

        for refused in &outcomes[..3] {
            assert!(
                refused.output.is_error,
                "{}: {:?}",
                refused.id, refused.output
            );
            let text = format!("{:?}", refused.output);
            assert!(text.contains("not available in this chat"), "{text}");
            assert!(
                !text.contains(memory.to_str().unwrap()),
                "the refusal repeats the path back: {text}"
            );
        }
        assert!(
            !outcomes[3].output.is_error,
            "a call that names nothing withheld runs: {:?}",
            outcomes[3].output
        );
    }

    struct PlainMutatingTool;

    impl Tool for PlainMutatingTool {
        fn name(&self) -> &'static str {
            "plain"
        }

        fn description(&self) -> &'static str {
            "takes a workspace permit from the executor"
        }

        fn concurrency(&self) -> ToolConcurrency {
            ToolConcurrency::Barrier
        }

        fn workspace_access(&self) -> WorkspaceAccess {
            WorkspaceAccess::Mutating
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        fn run(&self, _input: serde_json::Value, _ctx: ToolContext) -> super::super::ToolFuture {
            Box::pin(async { ToolOutput::text("done") })
        }
    }

    /// Run one mutating tool while something outside the step holds the
    /// checkout: a detached task or a background bash.
    async fn run_behind_a_held_checkout(tool: Arc<dyn Tool>) -> (ToolOutput, Vec<String>) {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::root(dir.path().to_path_buf());
        let _held = ctx.workspace.acquire_lease(WorkspaceAccess::Mutating).await;
        let started = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = started.clone();
        let outcomes = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            execute_calls_observed(
                vec![ToolCall {
                    id: "held-1".into(),
                    name: tool.name().into(),
                    input: serde_json::json!({}),
                }],
                move |_| Some(tool.clone()),
                Vec::new(),
                ctx,
                CancellationToken::new(),
                move |id, _| observed.lock().unwrap().push(id),
                |_, _| {},
            ),
        )
        .await
        .expect("a call behind a held checkout answers at once instead of waiting");
        let started = started.lock().unwrap().clone();
        (outcomes[0].output.clone(), started)
    }

    /// Nothing in the step can hold the lease against a barrier, so the
    /// holder is detached — and waiting for it is waiting for something
    /// the model already chose not to wait for. Both branches, the
    /// permit one (bash, service) and the lease one (edit, write).
    #[tokio::test]
    async fn a_held_checkout_refuses_at_once_and_runs_nothing() {
        for tool in [
            Arc::new(PlainMutatingTool) as Arc<dyn Tool>,
            Arc::new(ManagedLeaseTool),
        ] {
            let name = tool.name();
            let (output, started) = run_behind_a_held_checkout(tool).await;
            assert!(output.is_error, "{name}: {}", output.content);
            assert!(
                output.content.starts_with(&format!("{name}: ")),
                "{}",
                output.content
            );
            assert!(output.content.contains("held"), "{}", output.content);
            assert!(
                output.content.contains("notification"),
                "{}",
                output.content
            );
            assert!(started.is_empty(), "{name} was observed starting");
        }
    }

    /// A concurrent mutator of the step's own: the one holder worth
    /// waiting for, since it finishes inside this step.
    struct SiblingMutator {
        name: &'static str,
        managed: bool,
        hold: std::time::Duration,
    }

    impl Tool for SiblingMutator {
        fn name(&self) -> &'static str {
            self.name
        }

        fn description(&self) -> &'static str {
            "a concurrent mutator"
        }

        fn concurrency(&self) -> ToolConcurrency {
            ToolConcurrency::Concurrent
        }

        fn workspace_access(&self) -> WorkspaceAccess {
            WorkspaceAccess::Mutating
        }

        fn manages_workspace_access(&self) -> bool {
            self.managed
        }

        fn accepts_executor_workspace_lease(&self) -> bool {
            self.managed
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        fn run(&self, _input: serde_json::Value, _ctx: ToolContext) -> super::super::ToolFuture {
            let hold = self.hold;
            Box::pin(async move {
                tokio::time::sleep(hold).await;
                ToolOutput::text("done")
            })
        }
    }

    /// Run a waiter beside a sibling that holds the checkout, and return
    /// what the waiter's row said while it waited.
    async fn row_behind_a_sibling(managed: bool) -> Option<String> {
        let dir = tempfile::tempdir().unwrap();
        let tails = Arc::new(std::sync::Mutex::new(std::collections::HashMap::<
            String,
            String,
        >::new()));
        let (wake, _wake_rx) = tokio::sync::mpsc::channel(4);
        let mut ctx = ToolContext::root(dir.path().to_path_buf());
        ctx.output_tail = Some(super::super::OutputTailSink::new(tails.clone(), wake));
        let holder: Arc<dyn Tool> = Arc::new(SiblingMutator {
            name: "holder",
            managed: false,
            hold: std::time::Duration::from_millis(150),
        });
        let waiter: Arc<dyn Tool> = Arc::new(SiblingMutator {
            name: "waiter",
            managed,
            hold: std::time::Duration::ZERO,
        });
        let outcomes = execute_calls_observed(
            vec![
                ToolCall {
                    id: "holder-1".into(),
                    name: "holder".into(),
                    input: serde_json::json!({}),
                },
                ToolCall {
                    id: "waiter-1".into(),
                    name: "waiter".into(),
                    input: serde_json::json!({}),
                },
            ],
            move |name| {
                Some(if name == "holder" {
                    holder.clone()
                } else {
                    waiter.clone()
                })
            },
            Vec::new(),
            ctx,
            CancellationToken::new(),
            |_, _| {},
            |_, _| {},
        )
        .await;
        for outcome in &outcomes {
            assert!(!outcome.output.is_error, "{}", outcome.output.content);
        }
        tails.lock().unwrap().get("waiter-1").cloned()
    }

    /// Both waiting branches say the same thing: a silent row reads as a
    /// hang, and docs/agents-and-skills.md promises the row names itself.
    #[tokio::test]
    async fn a_sibling_holding_the_checkout_is_waited_for_by_name() {
        for managed in [false, true] {
            assert_eq!(
                row_behind_a_sibling(managed).await.as_deref(),
                Some(super::super::WORKSPACE_WAIT_NOTICE),
                "managed: {managed}"
            );
        }
    }

    #[tokio::test]
    async fn completion_observer_only_pairs_with_a_started_execution() {
        let dir = tempfile::tempdir().unwrap();
        let tool: Arc<dyn Tool> = Arc::new(RejectedBeforeStartTool);
        let lifecycle = Arc::new(std::sync::Mutex::new(Vec::new()));
        let started_lifecycle = lifecycle.clone();
        let completed_lifecycle = lifecycle.clone();

        let outcomes = execute_calls_observed(
            vec![ToolCall {
                id: "rejected-1".into(),
                name: "rejected".into(),
                input: serde_json::json!({}),
            }],
            move |_| Some(tool.clone()),
            Vec::new(),
            ToolContext::root(dir.path().to_path_buf()),
            CancellationToken::new(),
            move |id, _| {
                started_lifecycle
                    .lock()
                    .unwrap()
                    .push(format!("start:{id}"))
            },
            move |id, _| {
                completed_lifecycle
                    .lock()
                    .unwrap()
                    .push(format!("complete:{id}"));
            },
        )
        .await;

        assert!(outcomes[0].output.is_error);
        assert!(lifecycle.lock().unwrap().is_empty());
    }
}
