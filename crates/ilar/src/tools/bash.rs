//! bash: async shell command with cwd, timeout, output capture.

use serde::Deserialize;

use super::{Tool, ToolContext, ToolFuture, ToolKind, ToolOutput, parse_input};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_OUTPUT: usize = 100 * 1024;

#[derive(Clone, Default)]
struct Captured {
    retained: Vec<u8>,
    total: usize,
    error: Option<String>,
}

async fn drain<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    captured: std::sync::Arc<std::sync::Mutex<Captured>>,
) {
    use tokio::io::AsyncReadExt;

    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => {
                let mut state = captured.lock().unwrap();
                state.total = state.total.saturating_add(read);
                let keep = read.min(MAX_OUTPUT.saturating_sub(state.retained.len()));
                state.retained.extend_from_slice(&buffer[..keep]);
            }
            Err(error) => {
                captured.lock().unwrap().error = Some(error.to_string());
                return;
            }
        }
    }
}

struct DrainTask {
    handle: tokio::task::JoinHandle<()>,
    captured: std::sync::Arc<std::sync::Mutex<Captured>>,
}

impl DrainTask {
    fn spawn<R: tokio::io::AsyncRead + Unpin + Send + 'static>(reader: R) -> Self {
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Captured::default()));
        let handle = tokio::spawn(drain(reader, captured.clone()));
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

struct ProcessGroup(Option<u32>);

impl ProcessGroup {
    fn terminate(&self) {
        #[cfg(unix)]
        if let Some(pid) = self.0 {
            // The child starts a fresh process group whose id equals its pid.
            if let Ok(group) = i32::try_from(pid) {
                // SAFETY: `group` is a checked positive child pid and this
                // guard is armed only while that child still owns its group.
                unsafe {
                    libc::killpg(group, libc::SIGKILL);
                }
            }
        }
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.terminate();
    }
}

fn render_output(out: Captured, err: Captured) -> String {
    let total = out.total.saturating_add(err.total);
    let mut retained = out.retained;
    retained.extend_from_slice(&err.retained);
    let mut content = String::from_utf8_lossy(&retained).into_owned();
    if let Some(error) = out.error {
        content.push_str(&format!("\n(stdout read error: {error})"));
    }
    if let Some(error) = err.error {
        content.push_str(&format!("\n(stderr read error: {error})"));
    }
    if total > MAX_OUTPUT || content.len() > MAX_OUTPUT {
        let mut end = MAX_OUTPUT.min(content.len());
        while !content.is_char_boundary(end) {
            end -= 1;
        }
        content.truncate(end);
        content.push_str(&format!(
            "\n…(output truncated at {end} rendered bytes from {total} raw bytes)"
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

    fn kind(&self) -> ToolKind {
        ToolKind::Mutating
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
                let future = run_command(
                    input.command,
                    ctx.cwd,
                    timeout + std::time::Duration::from_secs(1),
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
            run_command(input.command, ctx.cwd, timeout).await
        })
    }
}

fn run_command(
    command_text: String,
    cwd: std::path::PathBuf,
    timeout: std::time::Duration,
) -> ToolFuture {
    Box::pin(async move {
        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg(&command_text)
            .current_dir(&cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) => return ToolOutput::error(format!("bash: {e}")),
        };
        let mut group = ProcessGroup(child.id());
        let mut stdout = DrainTask::spawn(child.stdout.take().unwrap());
        let mut stderr = DrainTask::spawn(child.stderr.take().unwrap());
        let drain_grace = std::time::Duration::from_secs(1);
        let status = tokio::select! {
            status = child.wait() => status,
            _ = tokio::time::sleep(timeout) => {
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
