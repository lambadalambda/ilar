//! One request per process. Standard output is reserved for the JSON response.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{
    SeenFiles, SeenFilesTransfer, Tool, ToolContext, ToolOutput, WorkspaceAccess, WorkspaceCoverage,
};
use crate::sandbox::SandboxConfig;
use crate::session::ImageContent;

const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct Request {
    tool: String,
    input: Value,
    cwd: PathBuf,
    vision: bool,
    withheld: Vec<PathBuf>,
    seen: SeenFilesTransfer,
}

#[derive(Serialize, Deserialize)]
struct Response {
    content: String,
    is_error: bool,
    images: Vec<ImageContent>,
    updates: SeenFilesTransfer,
}

fn file_tool(name: &str) -> anyhow::Result<Box<dyn Tool>> {
    Ok(match name {
        "read" => Box::new(super::read::ReadTool),
        "write" => Box::new(super::write::WriteTool),
        "edit" => Box::new(super::edit::EditTool),
        "glob" => Box::new(super::glob::GlobTool),
        "grep" => Box::new(super::grep::GrepTool),
        _ => bail!("unsupported file tool: {name}"),
    })
}

fn strip_sandbox(input: &mut Value) -> anyhow::Result<Option<super::sandbox_grants::Request>> {
    input
        .as_object_mut()
        .and_then(|fields| fields.remove("sandbox"))
        .map(serde_json::from_value)
        .transpose()
        .context("invalid sandbox request")
}

/// Run an approved direct-file call using only the helper beside this executable.
/// Missing helpers and transport failures never fall back to host-side execution.
pub async fn run(tool: &str, input: Value, ctx: &ToolContext) -> ToolOutput {
    let result = async {
        let executable = std::env::current_exe().context("locate file worker")?;
        let worker = executable
            .parent()
            .context("executable has no directory")?
            .join(format!("ilar-file-worker{}", std::env::consts::EXE_SUFFIX));
        run_at(tool, input, ctx, &worker).await
    }
    .await;
    result.unwrap_or_else(|error| ToolOutput::error(format!("{tool}: file worker: {error:#}")))
}

/// Explicit helper path for transport integration tests. Normal calls use `run`.
#[doc(hidden)]
pub async fn run_with_program(
    tool: &str,
    input: Value,
    ctx: &ToolContext,
    worker: &Path,
) -> ToolOutput {
    run_at(tool, input, ctx, worker)
        .await
        .unwrap_or_else(|error| ToolOutput::error(format!("{tool}: file worker: {error:#}")))
}

async fn run_at(
    tool: &str,
    mut input: Value,
    ctx: &ToolContext,
    worker: &Path,
) -> anyhow::Result<ToolOutput> {
    let implementation = file_tool(tool)?;
    let request = strip_sandbox(&mut input)?;
    let detail = serde_json::to_string(&input)?;
    let mut config = super::sandbox_grants::resolve(ctx, request.as_ref(), tool, &detail)
        .await
        .map_err(anyhow::Error::msg)?;
    // Output files are inspectable, but the worker cannot create spills here.
    if config.enabled
        && let Some(spill_dir) = &ctx.spill_dir
    {
        config.read_paths.push(spill_dir.clone());
    }
    ensure!(worker.is_absolute(), "file worker path must be absolute");
    ensure!(
        worker.is_file(),
        "missing file worker: {}",
        worker.display()
    );
    if let Some(refusal) = ctx.withheld_refusal(&input) {
        return Ok(ToolOutput::error(refusal));
    }
    let lease = if implementation.workspace_access() == WorkspaceAccess::Mutating {
        Some(match ctx.workspace_coverage(WorkspaceAccess::Mutating) {
            WorkspaceCoverage::Covered => ctx
                .workspace_lease
                .clone()
                .context("missing inherited lease")?,
            WorkspaceCoverage::Absent => {
                ctx.workspace.acquire_lease(WorkspaceAccess::Mutating).await
            }
            WorkspaceCoverage::Incompatible => {
                bail!("workspace access is not covered by its inherited lease")
            }
        })
    } else {
        None
    };
    let request = Request {
        tool: tool.into(),
        input,
        cwd: ctx.cwd.clone(),
        vision: ctx.vision,
        withheld: ctx.withheld.to_vec(),
        seen: ctx.seen_files.snapshot(),
    };
    let worker = worker.to_owned();
    let cancel = ctx.cancel.clone();
    let seen = ctx.seen_files.clone();
    // Dropping the caller closes this sender; the supervisor kills and reaps
    // the process before releasing its lease. Never abort the supervisor.
    let (caller, mut dropped) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let _lease = lease;
        let mut command = worker_command(&request.cwd, &config, &worker)?;
        let mut child = command.spawn().context("spawn file worker")?;
        let mut stdin = child.stdin.take().context("missing worker stdin")?;
        let stdout = child.stdout.take().context("missing worker stdout")?;
        let exchange = async {
            let send = async {
                stdin.write_all(&serde_json::to_vec(&request)?).await?;
                stdin.shutdown().await?;
                drop(stdin);
                Ok::<_, anyhow::Error>(())
            };
            let receive = async {
                let mut bytes = Vec::new();
                stdout
                    .take(MAX_RESPONSE_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)
                    .await?;
                ensure!(
                    bytes.len() <= MAX_RESPONSE_BYTES,
                    "worker response exceeds 16 MiB"
                );
                Ok::<_, anyhow::Error>(bytes)
            };
            let ((), bytes) = tokio::try_join!(send, receive)?;
            let status = child.wait().await?;
            ensure!(status.success(), "file worker exited with {status}");
            let response: Response =
                serde_json::from_slice(&bytes).context("invalid worker response")?;
            Ok::<_, anyhow::Error>(response)
        };
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(anyhow::anyhow!("file worker cancelled")),
            _ = &mut dropped => Err(anyhow::anyhow!("file worker caller dropped")),
            result = exchange => result,
        };
        // Also reap after broken pipes, oversized output and malformed protocol.
        if result.is_err() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        let response = result?;
        seen.merge_updates(response.updates);
        let mut output = ToolOutput::text(response.content).with_images(response.images);
        output.is_error = response.is_error;
        Ok(output)
    });
    let result = task.await.context("file worker supervisor failed")?;
    drop(caller);
    result
}

fn worker_command(
    cwd: &Path,
    config: &SandboxConfig,
    worker: &Path,
) -> anyhow::Result<tokio::process::Command> {
    let mut command = crate::sandbox::program(cwd, config, worker)?;
    // Keep the launcher's local cache environment across env_clear. These are
    // explicit policy overrides, not inherited credentials or loader hooks.
    let sandbox_env: Vec<_> = command
        .as_std()
        .get_envs()
        .filter_map(|(key, value)| value.map(|value| (key.to_owned(), value.to_owned())))
        .collect();
    // No inherited credentials, loader hooks, proxy settings or user PATH.
    command
        .env_clear()
        .envs(sandbox_env)
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .env("PATH", "/usr/bin:/bin")
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    // Ignore walkers use these locations to find global rules. Keep only the
    // paths; the kernel policy still decides whether their contents are readable.
    for key in ["HOME", "XDG_CONFIG_HOME"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    if !config.enabled {
        // Preserve the previous unsandboxed worker environment.
        for key in ["TMPDIR", "TMP", "TEMP"] {
            command.env(key, cwd);
        }
    }
    Ok(command)
}

async fn execute(request: Request) -> anyhow::Result<Response> {
    ensure!(
        request.input.get("sandbox").is_none(),
        "worker input contains sandbox request"
    );
    let tool = file_tool(&request.tool)?;
    let mut ctx = ToolContext::try_root(request.cwd)?;
    ctx.vision = request.vision;
    ctx.withheld = request.withheld.into();
    ctx.seen_files = SeenFiles::from_snapshot(request.seen);
    // Default context has no secrets, sandbox, approval channel or spill_dir.
    let output = if let Some(refusal) = ctx.withheld_refusal(&request.input) {
        ToolOutput::error(refusal)
    } else {
        tool.run(request.input, ctx.clone()).await
    };
    Ok(Response {
        content: output.content.clone(),
        is_error: output.is_error,
        images: output.images().to_vec(),
        updates: ctx.seen_files.updates(),
    })
}

/// Worker binary entry point. One unbounded request (write has no input cap),
/// followed by one bounded response; diagnostics must not use standard output.
pub async fn worker_main() -> anyhow::Result<()> {
    let mut bytes = Vec::new();
    tokio::io::stdin().read_to_end(&mut bytes).await?;
    let response =
        execute(serde_json::from_slice(&bytes).context("invalid worker request")?).await?;
    let bytes = serde_json::to_vec(&response)?;
    ensure!(
        bytes.len() <= MAX_RESPONSE_BYTES,
        "worker response exceeds 16 MiB"
    );
    let mut stdout = tokio::io::stdout();
    stdout.write_all(&bytes).await?;
    stdout.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::SeenFiles;
    use std::path::Path;

    #[cfg(target_os = "macos")]
    #[test]
    fn worker_restores_local_caches_after_clearing_inherited_environment() {
        let workspace = tempfile::tempdir().unwrap();
        let config = crate::sandbox::SandboxConfig {
            enabled: true,
            ..Default::default()
        };
        let child =
            super::worker_command(workspace.path(), &config, Path::new("/bin/cat")).unwrap();
        let expected = workspace
            .path()
            .canonicalize()
            .unwrap()
            .join(".ilar-cache/tmp");
        assert!(
            child
                .as_std()
                .get_envs()
                .any(|(k, v)| k == "TMPDIR" && v == Some(expected.as_os_str()))
        );
        assert!(
            child
                .as_std()
                .get_envs()
                .any(|(k, v)| k == "GOCACHE" && v.is_some())
        );
    }

    #[test]
    fn sandbox_request_is_removed_before_execution() {
        let mut input = serde_json::json!({"path": "a", "sandbox": {"reason": "reference", "read_paths": ["/reference"]}});
        assert!(super::strip_sandbox(&mut input).unwrap().is_some());
        assert_eq!(input, serde_json::json!({"path": "a"}));
    }

    #[test]
    fn response_protocol_preserves_image_bytes_and_errors() {
        let image = crate::session::ImageContent::png(&[0, 255, 1, 128]);
        let response = super::Response {
            content: "image".into(),
            is_error: true,
            images: vec![image.clone()],
            updates: SeenFiles::default().updates(),
        };
        let decoded: super::Response =
            serde_json::from_slice(&serde_json::to_vec(&response).unwrap()).unwrap();
        assert_eq!(decoded.images, [image]);
        assert!(decoded.is_error);
    }

    #[tokio::test]
    async fn worker_read_observation_allows_a_later_edit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.txt");
        std::fs::write(&path, "before").unwrap();
        let parent = SeenFiles::default();
        let request = |tool: &str, input| super::Request {
            tool: tool.into(),
            input,
            cwd: dir.path().to_owned(),
            vision: false,
            withheld: vec![],
            seen: parent.snapshot(),
        };
        let read = super::execute(request("read", serde_json::json!({"path": "note.txt"})))
            .await
            .unwrap();
        assert!(!read.is_error, "{}", read.content);
        assert!(read.content.contains("before"));
        parent.merge_updates(read.updates);
        let edit = super::execute(request(
            "edit",
            serde_json::json!({"path": "note.txt", "old_string": "before", "new_string": "after"}),
        ))
        .await
        .unwrap();
        assert!(!edit.is_error, "{}", edit.content);
        assert_eq!(std::fs::read_to_string(path).unwrap(), "after");
    }

    #[tokio::test]
    async fn worker_refuses_withheld_input_and_nested_sandbox_requests() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private.txt");
        std::fs::write(&path, "private data").unwrap();
        let response = super::execute(super::Request {
            tool: "read".into(),
            input: serde_json::json!({"path": path}),
            cwd: dir.path().to_owned(),
            vision: false,
            withheld: vec![path],
            seen: SeenFiles::default().snapshot(),
        })
        .await
        .unwrap();
        assert!(response.is_error);
        assert!(!response.content.contains("private data"));
        assert!(
            super::execute(super::Request {
                tool: "read".into(),
                input: serde_json::json!({"path": "x", "sandbox": null}),
                cwd: dir.path().to_owned(),
                vision: false,
                withheld: vec![],
                seen: SeenFiles::default().snapshot(),
            })
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn missing_helper_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = super::ToolContext::root(dir.path().to_owned());
        let output = super::run_with_program(
            "write",
            serde_json::json!({"path": "a", "content": "no"}),
            &ctx,
            &dir.path().join("missing-worker"),
        )
        .await;
        assert!(output.is_error);
        assert!(output.content.contains("missing file worker"));
        assert!(!dir.path().join("a").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropped_caller_reaps_worker_before_releasing_write_lease() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let worker = dir.path().join("worker");
        let pid_file = dir.path().join("pid");
        std::fs::write(&worker, "#!/bin/sh\necho $$ > pid\nexec /bin/sleep 30\n").unwrap();
        std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o700)).unwrap();
        let ctx = super::ToolContext::root(dir.path().to_owned());
        let scheduler = ctx.workspace.clone();
        let task = tokio::spawn(async move {
            super::run_with_program(
                "write",
                serde_json::json!({"path": "unused", "content": "unused"}),
                &ctx,
                &worker,
            )
            .await
        });
        let pid: i32 = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(text) = std::fs::read_to_string(&pid_file)
                    && let Ok(pid) = text.trim().parse()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("worker did not start");
        assert!(
            scheduler
                .try_acquire_lease(super::WorkspaceAccess::Mutating)
                .is_none()
        );
        task.abort();
        let _ = task.await;
        let lease = tokio::time::timeout(
            Duration::from_secs(5),
            scheduler.acquire_lease(super::WorkspaceAccess::Mutating),
        )
        .await
        .expect("supervisor did not release lease");
        // A reaped child no longer has a process id, including as a zombie.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
        drop(lease);
    }

    #[test]
    fn worker_observations_merge_even_when_the_hash_is_unchanged() {
        let parent = SeenFiles::default();
        let path = Path::new("/missing/file-worker-test");
        parent.record(path, b"same");
        let worker = SeenFiles::from_snapshot(parent.snapshot());
        worker.record(path, b"same");
        let updates = worker.updates();
        assert_eq!(updates.digests.len(), 1);
        parent.merge_updates(updates);
        assert_eq!(parent.digest_of(path), worker.digest_of(path));
    }

    #[test]
    fn compaction_rejects_late_worker_observations() {
        let parent = SeenFiles::default();
        let worker = SeenFiles::from_snapshot(parent.snapshot());
        let path = Path::new("/missing/file-worker-test");
        worker.record(path, b"old");
        parent.forget_after_compaction(Some("new"));
        parent.merge_updates(worker.updates());
        assert_eq!(parent.digest_of(path), None);
    }
}
