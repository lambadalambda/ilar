//! Concurrency-barrier tool executor — see meta/issues/tool-executor-barrier.md.
//!
//! Concurrent tools may overlap within a provider step; a barrier tool runs
//! alone. Workspace read/write exclusion is enforced independently.
//! Execution is concurrent, results are returned in call order.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use futures::stream::{FuturesUnordered, StreamExt};
use tokio_util::sync::CancellationToken;

use super::{Tool, ToolConcurrency, ToolContext, ToolOutput, WorkspaceAccess, WorkspaceCoverage};

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
    let call_count = calls.len();
    let mut outcomes: Vec<Option<CallOutcome>> = calls.iter().map(|_| None).collect();
    let mut pending: VecDeque<ToolCall> = calls.into_iter().collect();
    let mut running_meta: Vec<Running> = Vec::new();
    // (idx, future) pairs wrapped into single futures — tuples of futures
    // don't implement Future.
    type RunningFuture = Pin<Box<dyn Future<Output = (usize, ToolOutput)> + Send>>;
    let mut running: FuturesUnordered<RunningFuture> = FuturesUnordered::new();
    let mut next_idx = 0usize;

    let cancelled =
        loop {
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
                let access = tool.workspace_access();
                let call_ctx = ctx.clone();
                let input = call.input;
                running.push(Box::pin(async move {
                let output =
                    if background || manages_workspace_access || access == WorkspaceAccess::None {
                        tool.run(input, call_ctx).await
                    } else {
                        match call_ctx.workspace_coverage(access) {
                            WorkspaceCoverage::Covered => tool.run(input, call_ctx).await,
                            WorkspaceCoverage::Absent => {
                                let _permit = call_ctx.workspace.acquire(access).await;
                                tool.run(input, call_ctx).await
                            }
                        WorkspaceCoverage::Incompatible => ToolOutput::error(format!(
                            "tool {} requests workspace access not covered by its inherited lease",
                            tool.name()
                        )),
                        }
                    };
                (idx, output)
            }));
            }

            if running.is_empty() {
                break false;
            }

            tokio::select! {
                maybe = running.next() => {
                    let Some((idx, output)) = maybe else { continue };
                    if let Some(pos) = running_meta.iter().position(|r| r.idx == idx) {
                        let meta = running_meta.remove(pos);
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
