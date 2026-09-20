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
                "path": {"type": "string", "description": super::PATH_DESCRIPTION},
                "content": {"type": "string", "description": "The whole file; to change part of one, use edit"}
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
                        "write: workspace access is not covered by its inherited lease",
                    );
                }
            };
            let cancel = ctx.cancel;
            let path = ctx.cwd.join(&display_path);
            let seen_files = ctx.seen_files.clone();
            let result = run_blocking_io(lease, move || {
                if cancel.is_cancelled() {
                    // The caller prefixes "write {path}: "; saying
                    // "write" again here reads as a stutter.
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "cancelled",
                    ));
                }
                // What was there before, so the result can tell creating
                // a file from replacing one — the model that meant to
                // append has no other way to notice. A stat that failed
                // for any reason but absence is its own answer: reading
                // it as "new file" reported a creation over whatever
                // the write then destroyed.
                let previous = previous_from(std::fs::metadata(&path));
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                crate::atomic_file::replace_cancellable(
                    &path,
                    &content,
                    crate::atomic_file::Mode::Preserve,
                    &cancel,
                )?;
                // Whoever wrote the file knows what is in it, so write
                // licenses the edits that follow it. (Write itself needs
                // no licence: whole content, nothing to match against.)
                seen_files.record(&path, &content);
                Ok(previous)
            })
            .await;

            match result {
                Ok(Previous::Was(was)) => ToolOutput::text(format!(
                    "overwrote {display_path} ({}, was {was})",
                    crate::text::plural(byte_len, "byte")
                )),
                Ok(Previous::Nothing) => ToolOutput::text(format!(
                    "wrote {display_path} ({})",
                    crate::text::plural(byte_len, "byte")
                )),
                // Said rather than guessed: "wrote" here would have
                // claimed a creation over a file that may well have
                // been there.
                Ok(Previous::Unreadable) => ToolOutput::text(format!(
                    "wrote {display_path} ({}); whether it already existed could not be read",
                    crate::text::plural(byte_len, "byte")
                )),
                Err(e) => ToolOutput::error(format!("write {display_path}: {e}")),
            }
        })
    }
}

/// What was at the path before the write. Three answers, not two: a
/// stat can fail without the file being absent, and collapsing that
/// into "new file" reported a creation over a replacement.
#[derive(Debug)]
enum Previous {
    Was(u64),
    Nothing,
    Unreadable,
}

/// Read a stat's outcome as one of the three. Absence is the only
/// failure that means the file was not there.
fn previous_from(meta: std::io::Result<std::fs::Metadata>) -> Previous {
    match meta {
        Ok(meta) => Previous::Was(meta.len()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Previous::Nothing,
        Err(_) => Previous::Unreadable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// "wrote path (N bytes)" reads the same whether the file was new or
    /// a hundred lines that are now gone; the second case says so.
    #[tokio::test]
    async fn a_write_says_whether_it_created_or_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = || ToolContext::root(dir.path().to_path_buf());
        let out = WriteTool
            .run(
                serde_json::json!({"path": "a.txt", "content": "one\n"}),
                ctx(),
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(out.content, "wrote a.txt (4 bytes)");

        let out = WriteTool
            .run(serde_json::json!({"path": "a.txt", "content": "x"}), ctx())
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(out.content, "overwrote a.txt (1 byte, was 4)");
    }

    /// A stat can fail without the file being absent. Reading that as
    /// "new file" claimed a creation over whatever the write then
    /// replaced.
    ///
    /// A unit, because the scenario is not reachable end to end: every
    /// stat failure that is not `NotFound` — EACCES, ENOTDIR, ELOOP —
    /// also fails the open that follows, so the tool errors out before
    /// it can report anything. What is left is EINTR and ESTALE, which
    /// a test cannot arrange. The arm is defensive; its job is to not
    /// claim a creation it cannot vouch for.
    #[test]
    fn a_stat_that_failed_is_not_a_new_file() {
        use std::io::{Error, ErrorKind};

        assert!(matches!(
            previous_from(Err(Error::from(ErrorKind::NotFound))),
            Previous::Nothing
        ));
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::Interrupted,
            ErrorKind::NotADirectory,
        ] {
            assert!(
                matches!(previous_from(Err(Error::from(kind))), Previous::Unreadable),
                "{kind:?} was read as a new file"
            );
        }
    }

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
