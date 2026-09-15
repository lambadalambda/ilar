//! The `sudo` tool: one command as root, after the person has read
//! that exact command and said yes. The ask goes through the secret
//! store's grant prompt as the pseudo-secret `root`; a stored
//! `SUDO_PASSWORD` is fed on stdin when the system wants one. Off by
//! default: `agent.sudo = true` installs it.

use serde::Deserialize;

use super::bash::{DEFAULT_TIMEOUT_MS, SpillTarget, run_command};
use super::process::ChildEnv;
use super::{Tool, ToolConcurrency, ToolContext, ToolFuture, ToolOutput, WorkspaceAccess};

pub struct SudoTool {
    /// The binary; `sudo` outside tests.
    binary: String,
}

impl Default for SudoTool {
    fn default() -> Self {
        Self::with_binary("sudo")
    }
}

impl SudoTool {
    pub fn with_binary(binary: &str) -> Self {
        Self {
            binary: binary.to_string(),
        }
    }
}

#[derive(Deserialize)]
struct Input {
    command: String,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    preview_bytes: Option<usize>,
}

/// One word, safe for `sh`: single-quoted, with any single quote
/// spliced as `'\''`.
pub fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// The shell line that runs `command` as root, and what stdin carries.
/// With a password: `sudo -S` reads it from stdin (empty prompt, so
/// nothing is printed for it). Without: `sudo -n`, which fails at once
/// rather than waiting for a prompt nobody can answer. The command
/// itself starts by closing stdin: when sudo did not need to read the
/// password (a NOPASSWD rule), it would otherwise still be there for
/// the root command to read.
pub fn invocation(
    binary: &str,
    command: &str,
    password: Option<&str>,
) -> (String, Option<Vec<u8>>) {
    let quoted = shell_quote(&format!("exec </dev/null\n{command}"));
    match password {
        Some(password) => (
            format!("{binary} -S -p '' -- sh -c {quoted}"),
            Some(format!("{password}\n").into_bytes()),
        ),
        None => (format!("{binary} -n -- sh -c {quoted}"), None),
    }
}

impl Tool for SudoTool {
    fn name(&self) -> &'static str {
        "sudo"
    }

    fn description(&self) -> &'static str {
        "Run one shell command as root. The user is shown the exact command \
         and asked before it runs; say in `reason` what it is for. Use it only \
         for what really needs root (package installs, system services, ports \
         under 1024); everything else goes through bash. Output is captured \
         and spilled like bash's."
    }

    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Barrier
    }

    fn workspace_access(&self) -> WorkspaceAccess {
        WorkspaceAccess::Mutating
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "The shell command, run as root with sh -c in the project cwd"},
                "reason": {"type": "string", "description": "Why this needs root; shown to the user with the command"},
                "timeout_ms": {"type": "integer", "description": "Kill after this many milliseconds (default 120000)"},
                "preview_bytes": {"type": "integer", "description": "Output size you expect; the inline preview is capped at this on success"}
            },
            "required": ["command", "reason"]
        })
    }

    fn run(&self, input: serde_json::Value, ctx: ToolContext) -> ToolFuture {
        let binary = self.binary.clone();
        Box::pin(async move {
            let input: Input = match super::parse_input(input, "sudo") {
                Ok(input) => input,
                Err(error) => return error,
            };
            if input.command.trim().is_empty() {
                return ToolOutput::error(
                    "sudo: command is empty; give the command to run as root",
                );
            }
            if let Some(timeout) = input.timeout_ms
                && timeout < 1000
            {
                return ToolOutput::error(format!(
                    "sudo: timeout_ms is in milliseconds and {timeout} is under a second"
                ));
            }
            let Some(secrets) = ctx.secrets.as_ref() else {
                return ToolOutput::error(
                    "sudo: this session has no secret store, so nobody can be asked for root",
                );
            };
            let approval = secrets
                .approve_root(
                    crate::secrets::Request {
                        tool: "sudo",
                        names: &[],
                        detail: &input.command,
                        session_id: &ctx.session_id,
                        tool_call_id: ctx.call_id.as_deref(),
                        cancel: &ctx.cancel,
                    },
                    input.reason.trim(),
                )
                .await;
            if let Err(error) = approval {
                return ToolOutput::error(format!("sudo: {error}"));
            }
            // Covered by the approval just given: the person said yes
            // to this command as root, and the password is how root
            // is reached here.
            // Held empty means "none needed", answered at the prompt.
            let password = match secrets.held_or_stored(crate::secrets::SUDO_PASSWORD) {
                Ok(password) => password.filter(|password| !password.is_empty()),
                Err(error) => return ToolOutput::error(format!("sudo: {error:#}")),
            };
            let (line, stdin) = invocation(&binary, &input.command, password.as_deref());
            let granted: Vec<crate::secrets::Granted> = password
                .map(|password| {
                    crate::secrets::Granted::new(crate::secrets::SUDO_PASSWORD, password)
                })
                .into_iter()
                .collect();
            let mut env = ChildEnv::shielded(Some(secrets), &[]);
            env.stdin = stdin;
            let timeout =
                std::time::Duration::from_millis(input.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
            let tail_reporter = ctx.call_id.clone().zip(ctx.output_tail.clone());
            let wanted_password = granted.is_empty();
            let spill = SpillTarget::from_context(&ctx);
            let mut output = run_command(
                "sudo",
                line,
                ctx.cwd,
                timeout,
                tail_reporter,
                spill,
                input.preview_bytes,
                env,
                granted,
            )
            .await;
            if output.is_error && output.content.contains("incorrect password attempt") {
                // Forgotten, so the next ask takes a new one instead of
                // failing the same way for the rest of the session.
                secrets.forget_held(crate::secrets::SUDO_PASSWORD);
                output.content.push_str(
                    "\n(sudo refused the password; it is forgotten, and the next ask takes a new one)",
                );
            } else if output.is_error && wanted_password && output.content.contains("password") {
                output.content.push_str(
                    "\n(sudo wanted a password and none was given; the user types one into the \
                     prompt, or stores one with: ilar secret set SUDO_PASSWORD)",
                );
            }
            output
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_is_one_quoted_word_and_the_password_rides_stdin() {
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        let (line, stdin) = invocation("sudo", "apt install 'rg'", None);
        assert_eq!(
            line,
            "sudo -n -- sh -c 'exec </dev/null\napt install '\\''rg'\\'''"
        );
        assert_eq!(stdin, None);
        let (line, stdin) = invocation("/usr/bin/sudo", "id", Some("hunter22"));
        assert_eq!(
            line,
            "/usr/bin/sudo -S -p '' -- sh -c 'exec </dev/null\nid'"
        );
        assert_eq!(stdin.as_deref(), Some(b"hunter22\n".as_slice()));
    }
}
