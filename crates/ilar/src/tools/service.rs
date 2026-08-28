//! service: managed long-running processes (dev servers etc.) — see
//! meta/issues/service-tool.md. Services are owned by a per-session
//! [`ServiceManager`]; dropping it kills every service's process group,
//! so nothing outlives the session.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Deserialize;

use super::process::{Captured, drain, kill_process_group, shell_command};
use super::{Tool, ToolConcurrency, ToolContext, ToolFuture, ToolOutput, WorkspaceAccess};

/// Combined stdout+stderr retained per service.
const MAX_SERVICE_OUTPUT: usize = 256 * 1024;
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
    /// only by an actual kill, so the group is never signalled twice.
    group: Option<u32>,
    output: Arc<Mutex<Captured>>,
    started: std::time::Instant,
    exited: Option<String>,
}

impl ServiceEntry {
    /// Poll liveness, recording the exit status when the child is done.
    fn refresh(&mut self) {
        if self.exited.is_some() {
            return;
        }
        if let Some(child) = self.child.as_mut()
            && let Ok(Some(status)) = child.try_wait()
        {
            self.exited = Some(exit_label(status));
            self.child = None;
        }
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

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["start", "status", "logs", "stop"]},
                "name": {"type": "string", "description": "Service name ([a-zA-Z0-9_-], max 64)"},
                "command": {"type": "string", "description": "Shell command (start only)"},
                "lines": {"type": "integer", "description": "Log lines to return (default 50, max 500)"}
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
                        return ToolOutput::error("service start requires name and command");
                    };
                    if !valid_name(&name) {
                        return ToolOutput::error(format!(
                            "invalid service name {name:?} (use [a-zA-Z0-9_-], max 64 chars)"
                        ));
                    }
                    {
                        let mut services = manager.services.lock().unwrap();
                        if let Some(existing) = services.get_mut(&name) {
                            existing.refresh();
                            if existing.running() {
                                return ToolOutput::error(format!(
                                    "service {name:?} is already running (pid group {:?}); stop it first",
                                    existing.group
                                ));
                            }
                            // The entry is about to be replaced, and with
                            // it the only handle on whatever the old shell
                            // left running.
                            existing.kill_group();
                        }
                    }
                    let mut child = match shell_command(&command, &ctx.cwd).spawn() {
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
                            None => ToolOutput::error(format!("no service named {name:?}")),
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
                        return ToolOutput::error("service logs requires name");
                    };
                    let lines = input
                        .lines
                        .unwrap_or(DEFAULT_LOG_LINES)
                        .clamp(1, MAX_LOG_LINES);
                    let mut services = manager.services.lock().unwrap();
                    let Some(entry) = services.get_mut(&name) else {
                        return ToolOutput::error(format!("no service named {name:?}"));
                    };
                    entry.refresh();
                    let output = entry.output.lock().unwrap();
                    let text = String::from_utf8_lossy(&output.retained);
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
                        return ToolOutput::error("service stop requires name");
                    };
                    let mut child = {
                        let mut services = manager.services.lock().unwrap();
                        let Some(entry) = services.get_mut(&name) else {
                            return ToolOutput::error(format!("no service named {name:?}"));
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
                        entry.exited = Some(label.clone());
                        entry.group = None;
                    }
                    ToolOutput::text(format!("stopped service {name:?} ({label})"))
                }
                action => ToolOutput::error(format!(
                    "unknown service action {action:?} (start, status, logs, stop)"
                )),
            }
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

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
        (Some(exit), _) => format!("{name}: stopped ({exit}) · was: {}", entry.command),
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
