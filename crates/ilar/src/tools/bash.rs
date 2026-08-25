//! bash: async shell command with cwd, timeout, output capture.

use serde::Deserialize;

use super::process::{Captured, ProcessGroup, drain, shell_command, tail};
use super::{
    Tool, ToolConcurrency, ToolContext, ToolFuture, ToolOutput, WorkspaceAccess, parse_input,
};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_OUTPUT: usize = 100 * 1024;
/// Share of the rendered cap stderr can always claim, however loud
/// stdout was: the diagnosis is usually there.
const MIN_STDERR_SHARE: usize = MAX_OUTPUT / 2;

struct DrainTask {
    handle: tokio::task::JoinHandle<()>,
    captured: std::sync::Arc<std::sync::Mutex<Captured>>,
}

impl DrainTask {
    fn spawn<R: tokio::io::AsyncRead + Unpin + Send + 'static>(reader: R) -> Self {
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Captured::default()));
        let handle = tokio::spawn(drain(reader, MAX_OUTPUT, captured.clone()));
        Self { handle, captured }
    }

    async fn finish(&mut self, grace: std::time::Duration) -> Captured {
        if tokio::time::timeout(grace, &mut self.handle).await.is_err() {
            self.handle.abort();
            let _ = (&mut self.handle).await;
        }
        self.captured.lock().unwrap().clone()
    }
}

impl Drop for DrainTask {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Split the rendered cap between the two streams: stderr keeps at least
/// [`MIN_STDERR_SHARE`] when it needs it, stdout takes whatever is left.
fn stream_budgets(stdout_len: usize, stderr_len: usize) -> (usize, usize) {
    let stderr_keep = stderr_len.min(MIN_STDERR_SHARE.max(MAX_OUTPUT.saturating_sub(stdout_len)));
    (stdout_len.min(MAX_OUTPUT - stderr_keep), stderr_keep)
}

fn render_output(out: Captured, err: Captured) -> String {
    let total = out.total.saturating_add(err.total);
    let (stdout_keep, stderr_keep) = stream_budgets(out.retained.len(), err.retained.len());
    let stdout_tail = tail(&out.retained, stdout_keep);
    let stderr_tail = tail(&err.retained, stderr_keep);
    let rendered = stdout_tail.len() + stderr_tail.len();
    let mut content = String::from_utf8_lossy(stdout_tail).into_owned();
    content.push_str(&String::from_utf8_lossy(stderr_tail));
    if let Some(error) = out.error {
        content.push_str(&format!("\n(stdout read error: {error})"));
    }
    if let Some(error) = err.error {
        content.push_str(&format!("\n(stderr read error: {error})"));
    }
    if total > rendered {
        content.push_str(&format!(
            "\n…(output truncated at {rendered} rendered bytes from {total} raw bytes; \
             kept the tail of each stream)"
        ));
    }
    content
}

fn exit_description(status: std::process::ExitStatus) -> String {
    if let Some(code) = status.code() {
        return format!("exit code {code}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return format!("signal {signal}");
        }
    }
    "unknown termination".into()
}

pub struct BashTool;

#[derive(Deserialize)]
struct Input {
    command: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    run_in_background: bool,
}

impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }

    fn description(&self) -> &'static str {
        "Run a shell command and return combined stdout/stderr with the \
         exit code. Runs in the project cwd."
    }

    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Barrier
    }

    fn workspace_access(&self) -> WorkspaceAccess {
        WorkspaceAccess::Mutating
    }

    fn supports_background(&self) -> bool {
        true
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "timeout_ms": {"type": "integer", "description": "Kill after this long (default 120000 foreground; configured default for background)"},
                "run_in_background": {"type": "boolean", "description": "Run detached and deliver the result as a notification"}
            },
            "required": ["command"]
        })
    }

    fn run(&self, input: serde_json::Value, ctx: ToolContext) -> ToolFuture {
        Box::pin(async move {
            let input: Input = match parse_input(input, "bash") {
                Ok(v) => v,
                Err(e) => return e,
            };
            if input.run_in_background {
                if ctx.has_workspace_lease() {
                    return ToolOutput::error(
                        "bash: background mutation is unavailable inside a leased child workspace",
                    );
                }
                let Some(spawner) = ctx.subagent.clone() else {
                    return ToolOutput::error("bash: background runtime is unavailable");
                };
                let timeout = input
                    .timeout_ms
                    .map(std::time::Duration::from_millis)
                    .unwrap_or_else(|| spawner.background_tool_timeout());
                let command_preview: String = input.command.chars().take(120).collect();
                let description = if command_preview.len() < input.command.len() {
                    format!("bash: {command_preview}…")
                } else {
                    format!("bash: {command_preview}")
                };
                let parent_session_id = ctx.session_id.clone();
                // Background jobs surface through notifications, not
                // live tool rows; no tail reporter.
                let future = run_command(
                    input.command,
                    ctx.cwd,
                    timeout + std::time::Duration::from_secs(1),
                    None,
                );
                return spawner
                    .spawn_background_tool(
                        parent_session_id,
                        description,
                        timeout,
                        future,
                        crate::tools::WorkspaceAccess::Mutating,
                        ctx.cancel.clone(),
                    )
                    .await;
            }
            let timeout =
                std::time::Duration::from_millis(input.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
            let tail_reporter = ctx.call_id.clone().zip(ctx.output_tail.clone());
            run_command(input.command, ctx.cwd, timeout, tail_reporter).await
        })
    }
}

/// Last chunk of combined live output for the running-tool display.
fn live_tail(stdout: &DrainTask, stderr: &DrainTask) -> String {
    const TAIL_BYTES: usize = 480;
    let mut bytes = stdout.captured.lock().unwrap().retained.clone();
    bytes.extend_from_slice(&stderr.captured.lock().unwrap().retained);
    let start = bytes.len().saturating_sub(TAIL_BYTES);
    let text = String::from_utf8_lossy(&bytes[start..]);
    if start > 0 {
        format!("…{text}")
    } else {
        text.into_owned()
    }
}

fn run_command(
    command_text: String,
    cwd: std::path::PathBuf,
    timeout: std::time::Duration,
    tail_reporter: Option<(String, crate::tools::OutputTailSink)>,
) -> ToolFuture {
    Box::pin(async move {
        let mut child = match shell_command(&command_text, &cwd).spawn() {
            Ok(c) => c,
            Err(e) => return ToolOutput::error(format!("bash: {e}")),
        };
        let mut group = ProcessGroup(child.id());
        let mut stdout = DrainTask::spawn(child.stdout.take().unwrap());
        let mut stderr = DrainTask::spawn(child.stderr.take().unwrap());
        let drain_grace = std::time::Duration::from_secs(1);
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(500));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let status = loop {
            tokio::select! {
                status = child.wait() => break status,
                _ = &mut deadline => {
                    group.terminate();
                    child.start_kill().ok();
                    let _ = child.wait().await;
                    let out = stdout.finish(drain_grace).await;
                    let err = stderr.finish(drain_grace).await;
                    group.disarm();
                    return ToolOutput::error(format!(
                        "bash: timed out after {}ms\ncommand: {}\n{}",
                        timeout.as_millis(),
                        command_text,
                        render_output(out, err),
                    ));
                }
                _ = ticker.tick() => {
                    if let Some((call_id, sink)) = &tail_reporter {
                        let tail = live_tail(&stdout, &stderr);
                        if !tail.is_empty() {
                            sink.report(call_id, tail);
                        }
                    }
                }
            }
        };
        // A shell can exit after daemonizing children; do not let those
        // descendants outlive an apparently completed tool call.
        group.terminate();
        group.disarm();
        let out = stdout.finish(drain_grace).await;
        let err = stderr.finish(drain_grace).await;
        let mut content = render_output(out, err);
        match status {
            Ok(status) if status.success() => {
                content.push_str("\n(exit 0)");
                ToolOutput::text(content)
            }
            Ok(status) => {
                content.push_str(&format!("\n({})", exit_description(status)));
                ToolOutput::error(content)
            }
            Err(e) => ToolOutput::error(format!("bash: {e}\n{content}")),
        }
    })
}
