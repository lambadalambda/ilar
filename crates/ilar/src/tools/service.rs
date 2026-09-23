//! service: managed long-running processes (dev servers etc.) — see
//! meta/issues/service-tool.md. Services are owned by a per-session
//! [`ServiceManager`]; dropping it kills every service's process group,
//! so nothing outlives the session.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Deserialize;

use super::process::{
    Captured, ChildEnv, drain, kill_process_group, process_group_signalable, shell_command,
};
use super::{Tool, ToolConcurrency, ToolContext, ToolFuture, ToolOutput, WorkspaceAccess};

/// Combined stdout+stderr retained per service.
const MAX_SERVICE_OUTPUT: usize = 256 * 1024;
/// What an exited service's capture is cut down to. Enough that the
/// `logs` action can still answer its own documented maximum — 500
/// lines — for the service that just died, and small enough that a
/// session full of dead ones is not carrying a quarter megabyte each.
const RETAINED_AFTER_EXIT: usize = 64 * 1024;
const DEFAULT_LOG_LINES: usize = 50;
const MAX_LOG_LINES: usize = 500;
const STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

struct ServiceEntry {
    command: String,
    /// Consumed by `stop`; `None` once the child has been waited on.
    child: Option<tokio::process::Child>,
    /// The child's process group (equals its pid; it starts a new group).
    /// Kept after the direct child exits: a service that daemonizes
    /// (`node server.js &`) leaves the shell dead and the server alive in
    /// that same group, and this id is the only handle on it. Cleared
    /// by a kill, so the group is never signalled twice — and by
    /// `refresh` once the group answers nothing, so a recycled id is
    /// never signalled at all.
    group: Option<u32>,
    output: Arc<Mutex<Captured>>,
    /// What the service was started with; its logs are redacted of
    /// these values on the way out.
    granted: Vec<crate::secrets::Granted>,
    started: std::time::Instant,
    exited: Option<String>,
}

impl ServiceEntry {
    /// Poll liveness, recording the exit status when the child is done.
    fn refresh(&mut self) {
        if self.exited.is_none()
            && let Some(child) = self.child.as_mut()
            && let Ok(Some(status)) = child.try_wait()
        {
            let label = exit_label(status);
            self.mark_exited(label);
        }
        // Again on every status read, not once: the drain tasks hold
        // the same buffer and keep appending after the child is
        // reaped. A service that daemonizes — `node server.js &` —
        // leaves the shell dead in milliseconds and the server filling
        // the buffer for hours, so trimming only at the transition
        // would have trimmed an empty buffer and never looked again.
        if self.exited.is_some() {
            self.trim_output();
        }
        // The group is kept past the shell's death on purpose — a
        // daemonized grandchild lives in it, and this id is the only
        // handle on it. But once the group answers nothing, its id is
        // back in the kernel's pool, and a stop hours later would
        // SIGKILL whoever holds it now. Refresh runs on every status
        // read, so the id is dropped long before a pid could wrap.
        if self.child.is_none()
            && let Some(pid) = self.group
            && !process_group_signalable(pid)
        {
            self.group = None;
        }
    }

    /// The one place a service becomes "exited", whichever way it
    /// ended: reaped by `refresh`, or stopped by name. The `stop`
    /// action used to set the field itself and skip the trim — and
    /// since the trim's guard was the transition, no later refresh
    /// could run it either, so the documented way to end a service was
    /// the one way its output was never released.
    fn mark_exited(&mut self, label: String) {
        self.exited = Some(label);
        self.child = None;
        self.trim_output();
    }

    /// Cut an exited service's capture down to its tail.
    ///
    /// A service that is gone holds its whole `MAX_SERVICE_OUTPUT` for
    /// the life of the process, and nothing ever released it: a session
    /// that started and stopped a dozen of them carried a few megabytes
    /// of dead servers' startup banners to the end. The tail is what
    /// anyone reads anyway — the reason it exited is at the bottom, not
    /// the top — and `total` keeps counting the whole thing, so `log`
    /// still says "earlier output dropped".
    fn trim_output(&mut self) {
        let Ok(mut output) = self.output.lock() else {
            return;
        };
        if output.retained.len() <= RETAINED_AFTER_EXIT {
            return;
        }
        output.retained = crate::text::tail_bytes(&output.retained, RETAINED_AFTER_EXIT).to_vec();
    }

    fn running(&self) -> bool {
        self.exited.is_none() && self.child.is_some()
    }

    fn kill_group(&mut self) {
        if let Some(pid) = self.group.take() {
            kill_process_group(pid);
        }
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
    }
}

fn exit_label(status: std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit {code}"),
        None => "killed by signal".to_string(),
    }
}

/// Session-scoped service registry. Dropping it terminates everything.
#[derive(Default)]
pub struct ServiceManager {
    services: Mutex<HashMap<String, ServiceEntry>>,
}

impl ServiceManager {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Number of currently running services (for the pending manager).
    pub fn running_count(&self) -> usize {
        let mut services = self.services.lock().unwrap();
        let mut count = 0;
        for entry in services.values_mut() {
            entry.refresh();
            if entry.running() {
                count += 1;
            }
        }
        count
    }

    /// The services still running, as (name, command), sorted by name —
    /// what a compaction hands the summarizer, so the next context knows
    /// which servers and watchers it already owns.
    pub fn running_services(&self) -> Vec<(String, String)> {
        let mut services = self.services.lock().unwrap();
        let mut rows: Vec<(String, String)> = services
            .iter_mut()
            .filter_map(|(name, entry)| {
                entry.refresh();
                entry
                    .running()
                    .then(|| (name.clone(), entry.command.clone()))
            })
            .collect();
        rows.sort();
        rows
    }

    /// (name, running, detail) rows for UI display, sorted by name.
    pub fn snapshot(&self) -> Vec<(String, bool, String)> {
        let mut services = self.services.lock().unwrap();
        let mut rows: Vec<(String, bool, String)> = services
            .iter_mut()
            .map(|(name, entry)| {
                entry.refresh();
                let detail = match &entry.exited {
                    Some(exit) => exit.clone(),
                    None => format!("up {}", format_uptime(entry.started.elapsed())),
                };
                (name.clone(), entry.running(), detail)
            })
            .collect();
        rows.sort_by(|(a, _, _), (b, _, _)| a.cmp(b));
        rows
    }

    /// Kill every running service's process group.
    pub fn stop_all(&self) {
        let mut services = self.services.lock().unwrap();
        for entry in services.values_mut() {
            entry.kill_group();
        }
    }
}

impl Drop for ServiceManager {
    fn drop(&mut self) {
        self.stop_all();
    }
}

pub struct ServiceTool {
    manager: Arc<ServiceManager>,
}

impl ServiceTool {
    pub fn new(manager: Arc<ServiceManager>) -> Self {
        Self { manager }
    }
}

#[derive(Deserialize)]
struct Input {
    action: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    lines: Option<usize>,
    /// Stored secrets to hand the command as environment variables
    /// (start only).
    #[serde(default)]
    secrets: Vec<String>,
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn format_uptime(elapsed: std::time::Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m{}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h{}m", seconds / 3_600, (seconds % 3_600) / 60)
    }
}

impl Tool for ServiceTool {
    fn name(&self) -> &'static str {
        "service"
    }

    fn description(&self) -> &'static str {
        "Manage long-running processes (dev servers, watchers). Actions: \
         start {name, command}, status [name], logs {name, lines?}, \
         stop {name}. Services keep running between tool calls and are \
         killed when the session ends. Use this instead of backgrounding \
         servers with bash."
    }

    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Barrier
    }

    fn workspace_access(&self) -> WorkspaceAccess {
        WorkspaceAccess::Mutating
    }

    /// Only `start` runs a command in the checkout. Asking after a
    /// service, reading its logs or stopping it touches none of it, and
    /// must work while a background job holds the checkout — that is
    /// exactly when a dev server's logs are worth reading.
    fn workspace_access_for(&self, input: &serde_json::Value) -> WorkspaceAccess {
        match input.get("action").and_then(serde_json::Value::as_str) {
            Some("status" | "logs" | "stop") => WorkspaceAccess::None,
            _ => WorkspaceAccess::Mutating,
        }
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["start", "status", "logs", "stop"]},
                "name": {"type": "string", "description": "Service name ([a-zA-Z0-9_-], max 64)"},
                "command": {"type": "string", "description": "Shell command (start only)"},
                "lines": {"type": "integer", "description": "Log lines to return (default 50, max 500)"},
                "secrets": {"type": "array", "items": {"type": "string"}, "description": "Names of stored secrets (see the secrets tool) to set as environment variables of the service (start only). The user is asked before each use unless they granted it."}
            },
            "required": ["action"]
        })
    }

    fn run(&self, input: serde_json::Value, ctx: ToolContext) -> ToolFuture {
        let manager = self.manager.clone();
        Box::pin(async move {
            let input: Input = match super::parse_input(input, "service") {
                Ok(input) => input,
                Err(error) => return error,
            };
            match input.action.as_str() {
                "start" => {
                    let (Some(name), Some(command)) = (input.name, input.command) else {
                        return ToolOutput::error("service: start requires name and command");
                    };
                    if command.trim().is_empty() {
                        return ToolOutput::error(
                            "service start: command is empty; give the command to run",
                        );
                    }
                    if !valid_name(&name) {
                        return ToolOutput::error(format!(
                            "service: invalid name {name:?} (use [a-zA-Z0-9_-], max 64 chars)"
                        ));
                    }
                    {
                        let mut services = manager.services.lock().unwrap();
                        if let Some(existing) = services.get_mut(&name) {
                            existing.refresh();
                            if existing.running() {
                                return ToolOutput::error(format!(
                                    "service {name}: already running{}; stop it first",
                                    existing
                                        .group
                                        .map(|pid| format!(" (process group {pid})"))
                                        .unwrap_or_default()
                                ));
                            }
                            // The entry is about to be replaced, and with
                            // it the only handle on whatever the old shell
                            // left running.
                            existing.kill_group();
                        }
                    }
                    let granted = match ctx
                        .grant_secrets(
                            "service",
                            &input.secrets,
                            &format!("service {name}: {command}"),
                        )
                        .await
                    {
                        Ok(granted) => granted,
                        Err(error) => return ToolOutput::error(error),
                    };
                    let env = ChildEnv::shielded(ctx.secrets.as_ref(), &granted);
                    let mut child = match shell_command(&command, &ctx.cwd, &env).spawn() {
                        Ok(child) => child,
                        Err(error) => {
                            return ToolOutput::error(format!("service {name}: {error}"));
                        }
                    };
                    // Both streams share one capture, so logs stay
                    // interleaved in arrival order.
                    let output = Arc::new(Mutex::new(Captured::default()));
                    if let Some(stdout) = child.stdout.take() {
                        tokio::spawn(drain(stdout, MAX_SERVICE_OUTPUT, output.clone()));
                    }
                    if let Some(stderr) = child.stderr.take() {
                        tokio::spawn(drain(stderr, MAX_SERVICE_OUTPUT, output.clone()));
                    }
                    let pid = child.id();
                    manager.services.lock().unwrap().insert(
                        name.clone(),
                        ServiceEntry {
                            command: command.clone(),
                            group: pid,
                            child: Some(child),
                            output,
                            started: std::time::Instant::now(),
                            exited: None,
                            granted,
                        },
                    );
                    ToolOutput::text(format!(
                        "started service {name:?} (pid {}): {command}\nCheck it with \
                         {{\"action\":\"status\",\"name\":\"{name}\"}} and \
                         {{\"action\":\"logs\",\"name\":\"{name}\"}}.",
                        pid.map(|pid| pid.to_string())
                            .unwrap_or_else(|| "unknown".into()),
                    ))
                }
                "status" => {
                    let mut services = manager.services.lock().unwrap();
                    match input.name {
                        Some(name) => match services.get_mut(&name) {
                            Some(entry) => {
                                entry.refresh();
                                ToolOutput::text(describe(&name, entry))
                            }
                            None => {
                                ToolOutput::error(format!("service: no service named {name:?}"))
                            }
                        },
                        None => {
                            if services.is_empty() {
                                return ToolOutput::text("no services");
                            }
                            let mut names: Vec<&String> = services.keys().collect();
                            names.sort();
                            let names: Vec<String> =
                                names.into_iter().map(String::to_owned).collect();
                            let mut report = Vec::new();
                            for name in names {
                                let entry = services.get_mut(&name).expect("listed key");
                                entry.refresh();
                                report.push(describe(&name, entry));
                            }
                            ToolOutput::text(report.join("\n"))
                        }
                    }
                }
                "logs" => {
                    let Some(name) = input.name else {
                        return ToolOutput::error("service: logs requires name");
                    };
                    let lines = input
                        .lines
                        .unwrap_or(DEFAULT_LOG_LINES)
                        .clamp(1, MAX_LOG_LINES);
                    let mut services = manager.services.lock().unwrap();
                    let Some(entry) = services.get_mut(&name) else {
                        return ToolOutput::error(format!("service: no service named {name:?}"));
                    };
                    entry.refresh();
                    let output = entry.output.lock().unwrap();
                    // Every stored value, not just the ones this
                    // service was started with: a service that prints
                    // somebody else's token must not hand it over here.
                    let stored = ctx
                        .secrets
                        .as_ref()
                        .map(|secrets| secrets.all())
                        .unwrap_or_default();
                    let text = crate::secrets::redact(
                        &String::from_utf8_lossy(&output.retained),
                        &crate::secrets::redaction_set(stored, &entry.granted),
                    );
                    let all: Vec<&str> = text.lines().collect();
                    let start = all.len().saturating_sub(lines);
                    let mut body = all[start..].join("\n");
                    if output.total > output.retained.len() || start > 0 {
                        body = format!("… (earlier output dropped)\n{body}");
                    }
                    if body.trim().is_empty() {
                        body = "(no output yet)".to_string();
                    }
                    if let Some(error) = &output.error {
                        body.push_str(&format!("\n(log capture error: {error})"));
                    }
                    ToolOutput::text(format!("{}\n\n{body}", describe(&name, entry)))
                }
                "stop" => {
                    let Some(name) = input.name else {
                        return ToolOutput::error("service: stop requires name");
                    };
                    let mut child = {
                        let mut services = manager.services.lock().unwrap();
                        let Some(entry) = services.get_mut(&name) else {
                            return ToolOutput::error(format!(
                                "service: no service named {name:?}"
                            ));
                        };
                        entry.refresh();
                        if !entry.running() {
                            // The shell may be gone while what it
                            // backgrounded is not: reap the group anyway.
                            entry.kill_group();
                            return ToolOutput::text(format!(
                                "service {name:?} already stopped ({})",
                                entry.exited.as_deref().unwrap_or("never started")
                            ));
                        }
                        entry.kill_group();
                        entry.child.take()
                    };
                    let label = match child.as_mut() {
                        Some(child) => match tokio::time::timeout(STOP_GRACE, child.wait()).await {
                            Ok(Ok(status)) => exit_label(status),
                            _ => "killed (did not report status)".into(),
                        },
                        None => "already gone".into(),
                    };
                    if let Some(entry) = manager.services.lock().unwrap().get_mut(&name) {
                        entry.mark_exited(label.clone());
                        entry.group = None;
                    }
                    ToolOutput::text(format!("stopped service {name:?} ({label})"))
                }
                action => ToolOutput::error(format!(
                    "service: unknown action {action:?} (start, status, logs, stop)"
                )),
            }
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// An exited service used to hold its whole 256 KiB capture for
    /// the life of the process. Asserted on the buffer rather than
    /// through `logs`, which is the only way to see it: the tail is
    /// preserved by design, so every observable the tool offers reads
    /// the same before and after the trim.
    #[test]
    fn an_exited_service_releases_all_but_the_tail() {
        let output = Arc::new(Mutex::new(Captured::default()));
        let fill = |bytes: usize| {
            let mut captured = output.lock().unwrap();
            captured.retained = vec![b'x'; bytes];
            captured.total += bytes;
        };
        fill(MAX_SERVICE_OUTPUT);
        let mut entry = ServiceEntry {
            command: "serve".into(),
            child: None,
            group: None,
            output: output.clone(),
            granted: Vec::new(),
            started: std::time::Instant::now(),
            exited: None,
        };

        // `stop` ends a service by name; it used to set the field
        // itself and skip the trim, which is the path the tool tells
        // the model to use.
        entry.mark_exited("exit 1".into());
        assert_eq!(output.lock().unwrap().retained.len(), RETAINED_AFTER_EXIT);
        // `total` is untouched, so `logs` still says the rest is gone.
        assert_eq!(output.lock().unwrap().total, MAX_SERVICE_OUTPUT);

        // The drain tasks hold the same buffer and keep appending after
        // the child is reaped — a service that daemonizes leaves the
        // shell dead in milliseconds and the server writing for hours.
        // Trimming only at the transition trimmed an empty buffer and
        // never looked again.
        fill(MAX_SERVICE_OUTPUT);
        entry.refresh();
        assert_eq!(
            output.lock().unwrap().retained.len(),
            RETAINED_AFTER_EXIT,
            "the capture grew back and was never trimmed again"
        );

        // A capture already under the cap is left exactly alone.
        output.lock().unwrap().retained = b"short".to_vec();
        entry.refresh();
        assert_eq!(output.lock().unwrap().retained, b"short");
    }

    fn alive(pid: i32) -> bool {
        // SAFETY: signal 0 only probes; it never delivers anything.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    async fn settle<F: Fn() -> bool>(condition: F) -> bool {
        for _ in 0..200 {
            if condition() {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        condition()
    }

    /// A held checkout refuses what would change it, and asking after a
    /// service changes nothing: its status and logs are what the model
    /// reaches for while a background build holds the checkout.
    #[tokio::test]
    async fn a_held_checkout_refuses_a_start_but_not_a_look() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::root(dir.path().to_path_buf());
        let _held = ctx
            .workspace
            .acquire_lease(crate::tools::WorkspaceAccess::Mutating)
            .await;
        let tool: std::sync::Arc<dyn Tool> =
            std::sync::Arc::new(ServiceTool::new(ServiceManager::new()));
        let call = |id: &str, input: serde_json::Value| crate::tools::executor::ToolCall {
            id: id.into(),
            name: "service".into(),
            input,
        };
        let outcomes = crate::tools::executor::execute_calls(
            vec![
                call("look", serde_json::json!({"action": "logs", "name": "web"})),
                call(
                    "start",
                    serde_json::json!({"action": "start", "name": "web", "command": "true"}),
                ),
            ],
            |_| Some(tool.clone()),
            ctx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;

        let look = &outcomes[0].output.content;
        assert!(!look.contains("held by another job"), "{look}");
        let start = &outcomes[1].output;
        assert!(start.is_error, "{}", start.content);
        assert!(
            start.content.contains("held by another job"),
            "{}",
            start.content
        );
    }

    /// The module promises nothing outlives the session, and a service
    /// that daemonizes is exactly the shape that would: `sh` exits the
    /// moment it has backgrounded the server, and the process group id
    /// is then the only handle left on what it started.
    #[tokio::test]
    async fn a_daemonized_service_still_dies_with_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("grandchild.pid");
        let manager = ServiceManager::new();
        let tool = ServiceTool::new(manager.clone());

        let started = tool
            .run(
                serde_json::json!({
                    "action": "start",
                    "name": "daemon",
                    // The shell backgrounds a server and exits, which is
                    // what `node server.js &` does.
                    "command": format!("sleep 120 & echo $! > {}", pid_file.display()),
                }),
                ToolContext::root(dir.path().to_path_buf()),
            )
            .await;
        assert!(!started.is_error, "{}", started.content);

        let recorded_pid = || {
            std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|text| text.trim().parse::<i32>().ok())
        };
        assert!(
            settle(|| recorded_pid().is_some()).await,
            "the grandchild never recorded its pid"
        );
        let pid = recorded_pid().unwrap();
        // The direct child (`sh`) is already gone; the poll that notices
        // must not throw away the group with it.
        assert!(
            settle(|| manager.running_count() == 0).await,
            "the shell was still running"
        );
        assert!(alive(pid), "the grandchild died on its own");

        manager.stop_all();

        assert!(
            settle(|| !alive(pid)).await,
            "the grandchild outlived the session"
        );
    }
}

fn describe(name: &str, entry: &ServiceEntry) -> String {
    match (&entry.exited, entry.group) {
        // The command it was started with has exited, but the group it
        // opened has not: a service that daemonizes leaves the shell
        // dead and the server running. Calling that "stopped" invites
        // a restart that then collides with what is still listening.
        (Some(exit), Some(pid)) => format!(
            "{name}: started process exited ({exit}) but its group is still running \
             (process group {pid}) · up {} · {}",
            format_uptime(entry.started.elapsed()),
            entry.command
        ),
        (Some(exit), None) => format!("{name}: stopped ({exit}) · was: {}", entry.command),
        (None, group) => format!(
            "{name}: running (pid {}) · up {} · {}",
            group
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "?".into()),
            format_uptime(entry.started.elapsed()),
            entry.command
        ),
    }
}
