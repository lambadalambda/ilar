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
    WorkspaceCoverage,
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
    execute_calls_observed(calls, resolve, ctx, cancel, |_, _| {}, |_, _| {}).await
}

#[allow(clippy::type_complexity)]
pub(crate) async fn execute_calls_observed<F, O, C>(
    calls: Vec<ToolCall>,
    resolve: F,
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
                    output: ToolOutput::error(format!("no such tool: {}", call.name)),
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
            let background = tool.supports_background()
                && call
                    .input
                    .get("run_in_background")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true);
            if background && call_count != 1 {
                outcomes[idx] = Some(CallOutcome {
                    id: call.id,
                    name: call.name,
                    output: ToolOutput::error(
                        "background tool calls must be the only tool call in a provider step",
                    ),
                    cancelled: false,
                });
                continue;
            }
            running_meta.push(Running {
                idx,
                id: call.id.clone(),
                name: call.name.clone(),
                concurrency,
            });
            let manages_workspace_access = tool.manages_workspace_access();
            let accepts_executor_workspace_lease = tool.accepts_executor_workspace_lease();
            let access = tool.workspace_access();
            let mut call_ctx = ctx.clone();
            call_ctx.call_id = Some(call.id.clone());
            let input = call.input;
            let on_start = on_start.clone();
            let started_id = call.id.clone();
            let started_name = call.name.clone();
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
                            let lease = call_ctx.workspace.acquire_lease(access).await;
                            call_ctx.workspace_lease = Some(lease);
                            tool.run_observed(input, call_ctx, start).await
                        }
                        WorkspaceCoverage::Incompatible => ToolOutput::error(format!(
                            "tool {} requests workspace access not covered by its inherited lease",
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
                            let _permit = call_ctx.workspace.acquire(access).await;
                            tool.run_observed(input, call_ctx, start).await
                        }
                        WorkspaceCoverage::Incompatible => ToolOutput::error(format!(
                            "tool {} requests workspace access not covered by its inherited lease",
                            tool.name()
                        )),
                    }
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
                    id,
                    name,
                    output: ToolOutput::error("cancelled"),
                    cancelled: true,
                }
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[tokio::test]
    async fn managed_tool_is_observed_only_after_its_workspace_lease_is_acquired() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ToolContext::root(dir.path().to_path_buf());
        let scheduler = ctx.workspace.clone();
        let held = scheduler.acquire_lease(WorkspaceAccess::Mutating).await;
        ctx.workspace = scheduler;
        let tool: Arc<dyn Tool> = Arc::new(ManagedLeaseTool);
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let execution = tokio::spawn(execute_calls_observed(
            vec![ToolCall {
                id: "managed-1".into(),
                name: "managed".into(),
                input: serde_json::json!({}),
            }],
            move |_| Some(tool.clone()),
            ctx,
            CancellationToken::new(),
            move |id, _| {
                let _ = started_tx.send(id);
            },
            |_, _| {},
        ));

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), started_rx.recv())
                .await
                .is_err(),
            "managed tool was observed before workspace acquisition"
        );
        drop(held);
        assert_eq!(started_rx.recv().await.as_deref(), Some("managed-1"));
        assert!(
            execution
                .await
                .unwrap()
                .iter()
                .all(|outcome| !outcome.output.is_error)
        );
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
