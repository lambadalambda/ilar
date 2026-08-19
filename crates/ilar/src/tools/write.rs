//! write: create/overwrite a file, creating parent directories.

use serde::Deserialize;

use super::{
    Tool, ToolConcurrency, ToolContext, ToolFuture, ToolOutput, WorkspaceAccess, WorkspaceCoverage,
    parse_input, run_blocking_io,
};

pub struct WriteTool;

#[derive(Deserialize)]
struct Input {
    path: String,
    content: String,
}

impl Tool for WriteTool {
    fn name(&self) -> &'static str {
        "write"
    }

    fn description(&self) -> &'static str {
        "Create or overwrite a file with the given content. Parent \
         directories are created as needed."
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
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "content": {"type": "string"}
            },
            "required": ["path", "content"]
        })
    }

    fn run(&self, input: serde_json::Value, ctx: ToolContext) -> ToolFuture {
        Box::pin(async move {
            let input: Input = match parse_input(input, "write") {
                Ok(v) => v,
                Err(e) => return e,
            };
            let display_path = input.path;
            let byte_len = input.content.len();
            let content = input.content.into_bytes();
            let lease = match ctx.workspace_coverage(WorkspaceAccess::Mutating) {
                WorkspaceCoverage::Covered => ctx
                    .workspace_lease
                    .expect("covered workspace access has a lease"),
                WorkspaceCoverage::Absent => {
                    ctx.workspace.acquire_lease(WorkspaceAccess::Mutating).await
                }
                WorkspaceCoverage::Incompatible => {
                    return ToolOutput::error(
                        "write requests workspace access not covered by its inherited lease",
                    );
                }
            };
            let cancel = ctx.cancel;
            let path = ctx.cwd.join(&display_path);
            let result = run_blocking_io(lease, move || {
                if cancel.is_cancelled() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "write cancelled",
                    ));
                }
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                crate::atomic_file::replace_cancellable(
                    &path,
                    &content,
                    crate::atomic_file::Mode::Preserve,
                    &cancel,
                )
            })
            .await;

            match result {
                Ok(()) => ToolOutput::text(format!("wrote {display_path} ({byte_len} bytes)")),
                Err(e) => ToolOutput::error(format!("write {display_path}: {e}")),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn filesystem_work_uses_the_blocking_pool() {
        let runtime_thread = std::thread::current().id();
        let lease = crate::tools::WorkspaceScheduler::new()
            .acquire_lease(WorkspaceAccess::Mutating)
            .await;
        let worker_thread = run_blocking_io(lease, || Ok(std::thread::current().id()))
            .await
            .unwrap();

        assert_ne!(runtime_thread, worker_thread);
    }

    #[tokio::test]
    async fn dropped_write_future_keeps_its_workspace_lease_until_io_stops() {
        let scheduler = crate::tools::WorkspaceScheduler::new();
        let lease = scheduler.acquire_lease(WorkspaceAccess::Mutating).await;
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let task = tokio::spawn(run_blocking_io(lease, move || {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(())
        }));
        started_rx.await.unwrap();

        task.abort();
        let _ = task.await;
        assert!(
            scheduler
                .try_acquire_lease(WorkspaceAccess::Mutating)
                .is_none(),
            "detached filesystem work released its workspace lease"
        );

        release_tx.send(()).unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            scheduler.acquire_lease(WorkspaceAccess::Mutating),
        )
        .await
        .expect("workspace lease was not released after filesystem work stopped");
    }
}
