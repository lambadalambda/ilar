//! The `sudo` tool: one command as root, after the person has read
//! that exact command and said yes. The ask goes through the secret
//! store's grant prompt as the pseudo-secret `root` — approval only.
//! Only after the yes does the tool find out whether sudo wants a
//! password: a held or stored `SUDO_PASSWORD` is used as it is,
//! otherwise `sudo -n true` decides, and only a system that fails that
//! probe is asked for a password, in its own prompt. Off by default:
//! `agent.sudo = true` installs it.

use serde::Deserialize;

use super::bash::{
    DEFAULT_TIMEOUT_MS, MIN_TIMEOUT_MS, PREVIEW_BYTES_DESCRIPTION, SpillTarget,
    TIMEOUT_MS_DESCRIPTION, run_command, short_timeout_refusal,
};
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

/// How many passwords one call may try before giving up, like sudo's
/// own three attempts.
const PASSWORD_TRIES: usize = 3;
/// How long the `sudo -n true` probe may take. It either answers at
/// once or the system is in no state to run the command either.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// What a refused password typed this session leaves behind.
const FORGOTTEN: &str = "sudo refused that password; it is forgotten";
/// What a refused stored one leaves behind: it is not ours to drop.
const STILL_STORED: &str = "sudo refused the stored SUDO_PASSWORD; it is still stored — \
                            ilar secret set SUDO_PASSWORD updates it";

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

/// Whether sudo runs here without a password: `<binary> -n true`
/// succeeds. Nothing of its output is kept — the only thing asked is
/// whether the person has to be bothered for a password at all. A probe
/// that hangs (a wedged sudo, a stuck NSS lookup) counts as "wants
/// one": the person can still answer, and nothing ran as root.
async fn passwordless(binary: &str, cwd: &std::path::Path, env: &ChildEnv) -> bool {
    let mut command = super::process::shell_command(&format!("{binary} -n true"), cwd, env);
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    match tokio::time::timeout(PROBE_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => status.success(),
        Ok(Err(_)) => false,
        Err(_) => {
            child.start_kill().ok();
            false
        }
    }
}

impl Tool for SudoTool {
    fn name(&self) -> &'static str {
        "sudo"
    }

    fn description(&self) -> &'static str {
        "Run one shell command as root. The user is shown the exact command \
         and asked before it runs, unless they granted root to this tool \
         already; say in `reason` what it is for. Use it only \
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
                "timeout_ms": {"type": "integer", "description": TIMEOUT_MS_DESCRIPTION},
                "preview_bytes": {"type": "integer", "description": PREVIEW_BYTES_DESCRIPTION}
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
                && timeout < MIN_TIMEOUT_MS
            {
                return ToolOutput::error(short_timeout_refusal("sudo", timeout));
            }
            let Some(secrets) = ctx.secrets.as_ref() else {
                return ToolOutput::error(
                    "sudo: this session has no secret store, so nobody can be asked for root",
                );
            };
            let request = crate::secrets::Request {
                tool: "sudo",
                names: &[],
                detail: &input.command,
                session_id: &ctx.session_id,
                tool_call_id: ctx.call_id.as_deref(),
                cancel: &ctx.cancel,
            };
            // The yes comes first, and on its own: what sudo wants for
            // it is nobody's business until it is given.
            if let Err(error) = secrets.approve_root(request, input.reason.trim()).await {
                return ToolOutput::error(format!("sudo: {error}"));
            }
            // Covered by the approval just given: the person said yes
            // to this command as root, and the password is how root is
            // reached here.
            let mut password = match secrets.held_or_stored(crate::secrets::SUDO_PASSWORD) {
                Ok(password) => password.filter(|password| !password.is_empty()),
                Err(error) => return ToolOutput::error(format!("sudo: {error:#}")),
            };
            let timeout =
                std::time::Duration::from_millis(input.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
            let run = |password: Option<&str>, stored: Vec<crate::secrets::Granted>| {
                let (line, stdin) = invocation(&binary, &input.command, password);
                let granted: Vec<crate::secrets::Granted> = password
                    .map(|password| {
                        crate::secrets::Granted::new(
                            crate::secrets::SUDO_PASSWORD,
                            password.to_string(),
                        )
                    })
                    .into_iter()
                    .collect();
                let mut env = ChildEnv::shielded_from(&stored, &[]);
                env.stdin = stdin;
                run_command(
                    "sudo",
                    line,
                    ctx.cwd.clone(),
                    timeout,
                    ctx.call_id.clone().zip(ctx.output_tail.clone()),
                    SpillTarget::from_context(&ctx),
                    input.preview_bytes,
                    env,
                    // The password rides stdin; the output is scrubbed
                    // of it and of every other value the store holds.
                    crate::secrets::redaction_set(stored, &granted),
                )
            };
            if password.is_none() {
                // Nothing known: ask the system before asking the
                // person. A NOPASSWD rule means no prompt at all.
                let stored = secrets.all();
                let env = ChildEnv::shielded_from(&stored, &[]);
                if passwordless(&binary, &ctx.cwd, &env).await {
                    return run(None, stored).await;
                }
                if !secrets.can_ask() {
                    return ToolOutput::error(format!(
                        "sudo: this system wants a password and nobody is here to type one; {}",
                        crate::secrets::STORE_PASSWORD
                    ));
                }
            }
            // What the last refusal was, for the re-ask and for the
            // result if the person gives up: `None` until sudo has
            // refused one.
            let mut refusal: Option<&'static str> = None;
            for attempt in 1..=PASSWORD_TRIES {
                let password = match password.take() {
                    Some(known) => known,
                    // An empty answer is refused at the prompt itself,
                    // so nothing empty arrives here.
                    None => match secrets.ask_password(request, refusal.is_some()).await {
                        Ok(Some(typed)) => {
                            secrets.hold(crate::secrets::SUDO_PASSWORD, &typed);
                            typed
                        }
                        Ok(None) => {
                            return ToolOutput::error(match refusal {
                                Some(refusal) => format!("sudo: no password given ({refusal})"),
                                None => "sudo: no password given".to_string(),
                            });
                        }
                        Err(error) => return ToolOutput::error(format!("sudo: {error}")),
                    },
                };
                let mut output = run(Some(&password), secrets.all()).await;
                if !(output.is_error && output.content.contains("incorrect password attempt")) {
                    return output;
                }
                // A password typed this session is forgotten, so the
                // next ask takes a new one instead of failing the same
                // way for the rest of the session. A stored one is not
                // ours to drop: say how to replace it.
                let note = if secrets.forget_held(crate::secrets::SUDO_PASSWORD) {
                    FORGOTTEN
                } else {
                    STILL_STORED
                };
                output.content.push_str(&format!("\n({note})"));
                refusal = Some(note);
                // Out of tries, or a stored password refused with
                // nobody to type another: the result is sudo's own,
                // with the note above, rather than a second failure.
                if attempt == PASSWORD_TRIES || !secrets.can_ask() {
                    return output;
                }
            }
            unreachable!("the loop returns on its last attempt")
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
