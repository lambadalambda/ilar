//! bash: async shell command with cwd, timeout, output capture.
//!
//! Output the model cannot afford to read whole is split in two: a small
//! tail preview goes back in the tool result, and everything the capture
//! held is written to a file the result names, so the next step is a
//! targeted grep instead of the same command run again.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::process::{Captured, ProcessGroup, drain, shell_command};
use super::{
    Tool, ToolConcurrency, ToolContext, ToolFuture, ToolOutput, WorkspaceAccess, parse_input,
};
use crate::text::{tail_bytes, truncate_chars_ellipsis};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// What the model is shown. Small on purpose: four unfiltered API dumps
/// once filled a 100 KiB cap with minified JSON and taught the model
/// nothing, so the bulk goes to disk instead of into the context window.
const MAX_PREVIEW: usize = 30 * 1024;
/// Share of the preview stderr can always claim, however loud stdout
/// was: the diagnosis is usually there.
const MIN_STDERR_SHARE: usize = MAX_PREVIEW / 2;
/// Per-stream in-memory retention, tail-biased like the preview. Far
/// above the preview so the spilled file is worth grepping;
/// `Captured.total` still counts every byte that came past it.
const MAX_CAPTURE: usize = 2 * 1024 * 1024;
/// Subdirectory of the state dir spilled outputs live in.
const SPILL_DIR_NAME: &str = "tool-output";
/// A spilled output is a crutch for the turn that produced it; a week
/// later nothing refers to it.
pub const SPILL_RETENTION: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 60 * 60);

struct DrainTask {
    handle: tokio::task::JoinHandle<()>,
    captured: std::sync::Arc<std::sync::Mutex<Captured>>,
}

impl DrainTask {
    fn spawn<R: tokio::io::AsyncRead + Unpin + Send + 'static>(reader: R) -> Self {
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Captured::default()));
        let handle = tokio::spawn(drain(reader, MAX_CAPTURE, captured.clone()));
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

/// Split the preview between the two streams: stderr keeps at least
/// [`MIN_STDERR_SHARE`] when it needs it, stdout takes whatever is left.
fn stream_budgets(stdout_len: usize, stderr_len: usize) -> (usize, usize) {
    let stderr_keep = stderr_len.min(MIN_STDERR_SHARE.max(MAX_PREVIEW.saturating_sub(stdout_len)));
    (stdout_len.min(MAX_PREVIEW - stderr_keep), stderr_keep)
}

/// Where one tool call's full output is written, when the runtime gave
/// this context a state directory to write it in.
struct SpillTarget {
    dir: PathBuf,
    session_id: String,
    call_id: String,
}

impl SpillTarget {
    /// `<session>-<call>.txt`: a provider's call id is only unique
    /// within one response, so the session is what keeps two sessions
    /// that were handed the same id apart. Either part may be missing —
    /// a context outside a session, a call outside a provider step.
    fn path(&self) -> PathBuf {
        let parts: Vec<String> = [&self.session_id, &self.call_id]
            .into_iter()
            .filter(|part| !part.is_empty())
            .map(|part| file_stem(part))
            .collect();
        let name = if parts.is_empty() {
            crate::session::new_id()
        } else {
            parts.join("-")
        };
        self.dir.join(format!("{name}.txt"))
    }
}

/// An identifier verbatim, minus anything that would make it a path
/// rather than part of a file name.
fn file_stem(id: &str) -> String {
    let stem: String = id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect();
    let stem = stem.trim_matches('.');
    if stem.is_empty() {
        crate::session::new_id()
    } else {
        stem.to_string()
    }
}

/// The bytes a spill file holds: both streams, labelled when both spoke,
/// so a grep hit can be attributed to one of them.
fn spill_body(stdout: &[u8], stderr: &[u8]) -> Vec<u8> {
    if stdout.is_empty() || stderr.is_empty() {
        return [stdout, stderr].concat();
    }
    let mut body = Vec::with_capacity(stdout.len() + stderr.len() + 32);
    body.extend_from_slice(b"=== stdout ===\n");
    body.extend_from_slice(stdout);
    if !stdout.ends_with(b"\n") {
        body.push(b'\n');
    }
    body.extend_from_slice(b"=== stderr ===\n");
    body.extend_from_slice(stderr);
    body
}

fn line_count(body: &[u8]) -> usize {
    if body.is_empty() {
        return 0;
    }
    body.iter().filter(|byte| **byte == b'\n').count() + usize::from(!body.ends_with(b"\n"))
}

/// Size for the hint, in the unit that reads honestly at that scale.
fn human_bytes(bytes: usize) -> String {
    const KIB: usize = 1024;
    const MIB: usize = KIB * KIB;
    if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else {
        format!("{} KiB", bytes.div_ceil(KIB))
    }
}

/// Write the capture and describe where it went. Never an error the tool
/// call fails on: the preview is still a result, and a note beats a
/// tool result the model cannot use at all.
async fn spill_note(target: &SpillTarget, out: &Captured, err: &Captured) -> String {
    let body = spill_body(&out.retained, &err.retained);
    let path = target.path();
    if let Err(error) = tokio::fs::create_dir_all(&target.dir).await {
        return format!("(could not save the full output: {error})");
    }
    if let Err(error) = tokio::fs::write(&path, &body).await {
        return format!("(could not save the full output: {error})");
    }
    let mut note = format!(
        "full output: {} ({}, {} lines) — grep or read it for what you need",
        path.display(),
        human_bytes(body.len()),
        line_count(&body),
    );
    // Retention is tail-biased, so a capture that overran says which end
    // of the raw output survived rather than implying all of it did.
    let retained = out.retained.len().saturating_add(err.retained.len());
    let total = out.total.saturating_add(err.total);
    if total > retained {
        note.push_str(&format!(
            "\n(that file holds the last {} of {total} raw bytes; the earlier output is gone — \
             filter at the source next time)",
            human_bytes(retained),
        ));
    }
    note
}

/// The model-facing preview, led by the pointer to the rest when the
/// output did not fit in it.
async fn render_output(out: Captured, err: Captured, spill: Option<&SpillTarget>) -> String {
    let total = out.total.saturating_add(err.total);
    let (stdout_keep, stderr_keep) = stream_budgets(out.retained.len(), err.retained.len());
    let stdout_tail = tail_bytes(&out.retained, stdout_keep);
    let stderr_tail = tail_bytes(&err.retained, stderr_keep);
    let rendered = stdout_tail.len() + stderr_tail.len();
    let mut content = String::from_utf8_lossy(stdout_tail).into_owned();
    content.push_str(&String::from_utf8_lossy(stderr_tail));
    if let Some(error) = &out.error {
        content.push_str(&format!("\n(stdout read error: {error})"));
    }
    if let Some(error) = &err.error {
        content.push_str(&format!("\n(stderr read error: {error})"));
    }
    if total <= rendered {
        return content;
    }
    content.push_str(&format!(
        "\n…(output truncated at {rendered} rendered bytes from {total} raw bytes; \
         kept the tail of each stream)"
    ));
    let Some(target) = spill else {
        return content;
    };
    // The pointer leads. The TUI shows a tool result head-first and cuts
    // it long before the end, so a hint appended after 30 KiB of preview
    // is one only the model would ever see — and the human is the reader
    // who has to decide whether the file is worth opening.
    let note = spill_note(target, &out, &err).await;
    format!("{note}\n{content}")
}

/// Directory spilled tool output lives in, under the state dir.
pub fn spill_dir(state_dir: &Path) -> PathBuf {
    state_dir.join(SPILL_DIR_NAME)
}

/// Remove spilled outputs last written before `cutoff`. Best effort by
/// construction: every error is swallowed, because a stale temporary
/// file is never worth failing a startup over.
fn remove_spills_before(dir: &Path, cutoff: std::time::SystemTime) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "txt") {
            continue;
        }
        let stale = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .is_some_and(|modified| modified < cutoff);
        if stale {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Sweep a spill directory of everything past [`SPILL_RETENTION`], as a
/// session starts.
pub fn clean_spills(dir: &Path) {
    let Some(cutoff) = std::time::SystemTime::now().checked_sub(SPILL_RETENTION) else {
        return;
    };
    remove_spills_before(dir, cutoff);
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
         exit code. Runs in the project cwd. Large output comes back as a \
         truncated tail preview and the full text is saved to a file the \
         result names, for grep or read — but filtering at the source \
         (jq, grep, head) is still cheaper than reading the spill back."
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
            // The provider's call id names the file, so a result and its
            // spill can be matched up after the fact; a call without one
            // (a background job) still gets a file, under a fresh id.
            let spill = ctx.spill_dir.clone().map(|dir| SpillTarget {
                dir,
                session_id: ctx.session_id.clone(),
                call_id: ctx.call_id.clone().unwrap_or_else(crate::session::new_id),
            });
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
                let description = format!("bash: {}", truncate_chars_ellipsis(&input.command, 120));
                let parent_session_id = ctx.session_id.clone();
                // Background jobs surface through notifications, not
                // live tool rows; no tail reporter.
                let future = run_command(
                    input.command,
                    ctx.cwd,
                    timeout + std::time::Duration::from_secs(1),
                    None,
                    spill,
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
            run_command(input.command, ctx.cwd, timeout, tail_reporter, spill).await
        })
    }
}

/// The tail of one stream's capture, and how much of it there was. Only
/// the tail is copied: the capture behind it runs to megabytes and this
/// is read twice a second while a command runs.
fn captured_tail(task: &DrainTask, keep: usize) -> (Vec<u8>, usize) {
    let captured = task.captured.lock().unwrap();
    (
        tail_bytes(&captured.retained, keep).to_vec(),
        captured.retained.len(),
    )
}

/// Last chunk of combined live output for the running-tool display.
fn live_tail(stdout: &DrainTask, stderr: &DrainTask) -> String {
    const TAIL_BYTES: usize = 480;
    let (mut bytes, stdout_len) = captured_tail(stdout, TAIL_BYTES);
    let (stderr_tail, stderr_len) = captured_tail(stderr, TAIL_BYTES);
    bytes.extend_from_slice(&stderr_tail);
    let start = bytes.len().saturating_sub(TAIL_BYTES);
    let shown = bytes.len() - start;
    let text = String::from_utf8_lossy(&bytes[start..]);
    // Against the captured byte count, not the decoded string's: lossy
    // decoding inflates every invalid byte threefold, and each stream
    // was already cut to the tail on the way in.
    if stdout_len + stderr_len > shown {
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
    spill: Option<SpillTarget>,
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
                    let rendered = render_output(out, err, spill.as_ref()).await;
                    return ToolOutput::error(format!(
                        "bash: timed out after {}ms\ncommand: {}\n{rendered}",
                        timeout.as_millis(),
                        command_text,
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
        let mut content = render_output(out, err, spill.as_ref()).await;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn spill_file(dir: &Path, name: &str, age: std::time::Duration) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"spilled\n").unwrap();
        let modified = std::time::SystemTime::now() - age;
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        path
    }

    /// A spill is a crutch for the turn that made it; a week later it is
    /// only disk. Fresh ones — and anything that is not a spill — stay.
    #[test]
    fn the_startup_sweep_removes_only_stale_spills() {
        let dir = tempfile::tempdir().unwrap();
        let day = std::time::Duration::from_secs(24 * 60 * 60);
        let stale = spill_file(dir.path(), "call-old.txt", SPILL_RETENTION + day);
        let fresh = spill_file(dir.path(), "call-new.txt", day);
        let foreign = spill_file(dir.path(), "notes.log", SPILL_RETENTION + day);

        remove_spills_before(dir.path(), std::time::SystemTime::now() - SPILL_RETENTION);

        assert!(!stale.exists(), "a week-old spill survived");
        assert!(fresh.exists(), "a fresh spill was removed");
        assert!(foreign.exists(), "the sweep touched a non-spill file");
    }

    /// Cleanup never fails a startup: a state dir that was never written
    /// to has no spill directory at all.
    #[test]
    fn sweeping_a_missing_directory_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        clean_spills(&dir.path().join("never-used"));
    }

    #[test]
    fn an_identifier_becomes_a_plain_file_name() {
        assert_eq!(file_stem("toolu_01ABC-def"), "toolu_01ABC-def");
        // Separators are gone and no leading dot survives, so the name
        // can only ever land in the spill directory itself.
        assert_eq!(file_stem("../../etc/passwd"), "_.._etc_passwd");
        assert!(!file_stem("").is_empty(), "an empty id still gets a file");
    }

    /// Provider call ids are unique within a response, not across
    /// sessions: two sessions handed the same one must not land on the
    /// same file.
    #[test]
    fn a_spill_is_named_for_its_session_and_its_call() {
        let target = |session: &str, call: &str| SpillTarget {
            dir: PathBuf::from("/spills"),
            session_id: session.into(),
            call_id: call.into(),
        };

        assert_eq!(
            target("01JABC", "call_1").path(),
            Path::new("/spills/01JABC-call_1.txt")
        );
        assert_ne!(
            target("01JABC", "call_1").path(),
            target("01JXYZ", "call_1").path()
        );
        // A context outside a session still gets a file, named for what
        // it does have.
        assert_eq!(target("", "call_1").path(), Path::new("/spills/call_1.txt"));
        assert!(target("", "").path().starts_with("/spills"));
    }

    /// A grep hit in a spill has to be attributable to a stream, but a
    /// command that only spoke on one of them needs no ceremony.
    #[test]
    fn both_streams_are_labelled_only_when_both_spoke() {
        assert_eq!(spill_body(b"out\n", b""), b"out\n");
        assert_eq!(spill_body(b"", b"err\n"), b"err\n");
        assert_eq!(
            spill_body(b"out", b"err\n"),
            b"=== stdout ===\nout\n=== stderr ===\nerr\n"
        );
    }

    #[test]
    fn line_and_size_reporting_match_the_bytes() {
        assert_eq!(line_count(b""), 0);
        assert_eq!(line_count(b"a\nb\n"), 2);
        assert_eq!(line_count(b"a\nb"), 2);
        assert_eq!(human_bytes(0), "0 KiB");
        assert_eq!(human_bytes(1), "1 KiB");
        assert_eq!(human_bytes(30 * 1024), "30 KiB");
        assert_eq!(human_bytes(2 * 1024 * 1024), "2.0 MiB");
    }

    /// Stderr keeps its share of the smaller preview, and stdout takes
    /// the rest — the same split as before, at the new size.
    #[test]
    fn stderr_keeps_its_share_of_the_preview() {
        assert_eq!(stream_budgets(10, 20), (10, 20));
        let (stdout, stderr) = stream_budgets(MAX_PREVIEW * 4, MAX_PREVIEW);
        assert_eq!(stderr, MIN_STDERR_SHARE);
        assert_eq!(stdout, MAX_PREVIEW - MIN_STDERR_SHARE);
        // A quiet stderr leaves the whole preview to stdout.
        assert_eq!(stream_budgets(MAX_PREVIEW * 4, 12), (MAX_PREVIEW - 12, 12));
    }

    #[tokio::test]
    async fn output_within_the_preview_is_never_spilled() {
        let dir = tempfile::tempdir().unwrap();
        let target = SpillTarget {
            dir: dir.path().join("tool-output"),
            session_id: "session-1".into(),
            call_id: "call-small".into(),
        };
        let out = Captured {
            retained: b"small\n".to_vec(),
            total: 6,
            error: None,
        };

        let rendered = render_output(out, Captured::default(), Some(&target)).await;

        assert_eq!(rendered, "small\n");
        assert!(!target.dir.exists(), "an untruncated result made a file");
    }

    /// A state directory that cannot be written to costs the model its
    /// spill, not its result: the preview and the truncation note stand,
    /// and the note does not name a file that is not there.
    #[tokio::test]
    async fn a_spill_that_cannot_be_written_still_returns_the_preview() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, b"").unwrap();
        let target = SpillTarget {
            dir: blocker.join("tool-output"),
            session_id: "session-1".into(),
            call_id: "call-blocked".into(),
        };
        let out = Captured {
            retained: vec![b'x'; MAX_PREVIEW + 10],
            total: MAX_PREVIEW + 10,
            error: None,
        };

        let rendered = render_output(out, Captured::default(), Some(&target)).await;

        assert!(
            rendered.starts_with("(could not save the full output:"),
            "{rendered}"
        );
        assert!(rendered.contains("\nxxx"), "the preview was lost");
        assert!(rendered.contains("output truncated at"), "{rendered}");
        assert!(!rendered.contains("full output: /"), "{rendered}");
    }
}
