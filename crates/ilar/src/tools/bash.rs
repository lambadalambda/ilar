//! bash: async shell command with cwd, timeout, output capture.

use serde::Deserialize;

use super::{Tool, ToolContext, ToolFuture, ToolKind, ToolOutput, parse_input};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_OUTPUT: usize = 100 * 1024;

pub struct BashTool;

#[derive(Deserialize)]
struct Input {
    command: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
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

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "timeout_ms": {"type": "integer", "description": "Kill after this long (default 120000)"}
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
            let timeout =
                std::time::Duration::from_millis(input.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
            let mut command = tokio::process::Command::new("sh");
            command
                .arg("-c")
                .arg(&input.command)
                .current_dir(&ctx.cwd)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true);
            let mut child = match command.spawn() {
                Ok(c) => c,
                Err(e) => return ToolOutput::error(format!("bash: {e}")),
            };
            let mut stdout = child.stdout.take().unwrap();
            let mut stderr = child.stderr.take().unwrap();
            let collect = async {
                use tokio::io::AsyncReadExt;
                let (mut out, mut err) = (String::new(), String::new());
                let _ = stdout.read_to_string(&mut out).await;
                let _ = stderr.read_to_string(&mut err).await;
                (out, err)
            };
            let status = tokio::select! {
                status = child.wait() => status,
                _ = tokio::time::sleep(timeout) => {
                    child.start_kill().ok();
                    let _ = child.wait().await;
                    return ToolOutput::error(format!(
                        "bash: timed out after {}ms\ncommand: {}",
                        timeout.as_millis(),
                        input.command
                    ));
                }
            };
            let (out, err) = collect.await;
            let mut content = format!("{out}{err}");
            if content.len() > MAX_OUTPUT {
                content = format!(
                    "{}\n…(output truncated, {} of {} bytes)",
                    &content[..MAX_OUTPUT],
                    MAX_OUTPUT,
                    content.len()
                );
            }
            match status {
                Ok(status) if status.success() => {
                    content.push_str("\n(exit 0)");
                    ToolOutput::text(content)
                }
                Ok(status) => {
                    content.push_str(&format!("\n(exit code {})", status.code().unwrap_or(-1)));
                    ToolOutput::error(content)
                }
                Err(e) => ToolOutput::error(format!("bash: {e}\n{content}")),
            }
        })
    }
}
