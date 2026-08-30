//! Shared child-process plumbing for the shell-backed tools (bash and
//! service): one spawn shape, one process-group kill, one capture
//! policy, so the two cannot drift apart again.

use std::sync::{Arc, Mutex};

/// Bounded capture of one stream. Retention is tail-biased: when a stream
/// overruns its cap the newest bytes win, because a failing command puts
/// its diagnosis at the end.
#[derive(Clone, Default)]
pub(crate) struct Captured {
    pub retained: Vec<u8>,
    /// Every byte read, including the ones retention dropped.
    pub total: usize,
    pub error: Option<String>,
}

impl Captured {
    fn push(&mut self, chunk: &[u8], cap: usize) {
        self.total = self.total.saturating_add(chunk.len());
        let keep = chunk.len().min(cap);
        self.retained
            .extend_from_slice(&chunk[chunk.len() - keep..]);
        if self.retained.len() > cap {
            self.retained.drain(..self.retained.len() - cap);
        }
    }
}

/// Read buffer for one stream, sized against its retention. Retention
/// moves every retained byte for each chunk that overflows it, so
/// 8 KiB reads against a megabyte cap turn a loud command into a
/// memmove benchmark. A short read still returns as soon as bytes are
/// there, so nothing about the live tail gets lazier.
fn read_buffer_len(cap: usize) -> usize {
    (cap / 16).clamp(8 * 1024, 256 * 1024)
}

/// Read `reader` to EOF into `captured`, keeping at most `cap` bytes.
pub(crate) async fn drain<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    cap: usize,
    captured: Arc<Mutex<Captured>>,
) {
    use tokio::io::AsyncReadExt;

    // Heap, not the stack: this future is spawned per stream and a
    // buffer this size inside it would be carried around whole.
    let mut buffer = vec![0u8; read_buffer_len(cap)];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => captured.lock().unwrap().push(&buffer[..read], cap),
            Err(error) => {
                captured.lock().unwrap().error = Some(error.to_string());
                return;
            }
        }
    }
}

/// `sh -c` in `cwd` with both pipes captured, no stdin, and its own
/// session so descendants can be killed as a unit.
///
/// A session, not just a process group: a child in the parent's session
/// keeps the controlling terminal, and a program that opens `/dev/tty`
/// (sudo's password prompt above all) bypasses the captured pipes,
/// scribbles over the TUI wherever the cursor sits, and then blocks the
/// call until its timeout. With no controlling terminal that open fails
/// fast, with an error the model can read and relay. A session leader
/// leads its own group too, so `killpg(pid)` reaping is unchanged.
pub(crate) fn shell_command(command_text: &str, cwd: &std::path::Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("sh");
    command
        .arg("-c")
        .arg(command_text)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    // SAFETY: setsid and setpgid are async-signal-safe; nothing here
    // allocates or locks between fork and exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                // Cannot happen after fork (a fresh child leads nothing),
                // but keep the kill-the-group guarantee if it somehow does.
                if libc::setpgid(0, 0) == -1 {
                    // Both failed: the child would run in ilar's own
                    // group, where `killpg` on its pid reaps nothing and
                    // a timeout leaves the process running for good. Fail
                    // the spawn instead — the caller reports an error the
                    // model can read.
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    command
}

/// Is this process group still ours to signal? Signal 0 asks the kernel
/// and delivers nothing. `ESRCH` means the group is empty — its id is
/// back in the pool and belongs to nobody; `EPERM` means it exists and
/// is somebody else's, which is an even better reason to leave it
/// alone. Either way the answer is no.
#[cfg(unix)]
pub(crate) fn process_group_signalable(pid: u32) -> bool {
    let Ok(group) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 performs permission and existence checks only.
    unsafe { libc::killpg(group, 0) == 0 }
}

/// No process groups to reason about: keep every holder's id exactly as
/// it was, since the kill below is a no-op anyway.
#[cfg(not(unix))]
pub(crate) fn process_group_signalable(_pid: u32) -> bool {
    true
}

/// SIGKILL the group led by a [`shell_command`] child: it starts a fresh
/// process group whose id equals its pid.
///
/// Probed first, because a group id outlives its group. Once the last
/// member exits the kernel may hand the id to an unrelated group, and
/// a session that held onto it would SIGKILL a stranger. The probe
/// closes the common case (the group is simply gone); holders narrow
/// the rest by dropping the id as soon as it stops answering.
#[cfg(unix)]
pub(crate) fn kill_process_group(pid: u32) {
    if !process_group_signalable(pid) {
        return;
    }
    if let Ok(group) = i32::try_from(pid) {
        // SAFETY: `group` is a checked positive child pid and callers
        // issue this only while that child still owns its group.
        unsafe {
            libc::killpg(group, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
pub(crate) fn kill_process_group(_pid: u32) {}

/// Kill-on-drop guard for a spawned child's process group.
pub(crate) struct ProcessGroup(pub Option<u32>);

impl ProcessGroup {
    pub fn terminate(&self) {
        if let Some(pid) = self.0 {
            kill_process_group(pid);
        }
    }

    pub fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn captured(chunks: &[&[u8]], cap: usize) -> Captured {
        let mut captured = Captured::default();
        for chunk in chunks {
            captured.push(chunk, cap);
        }
        captured
    }

    #[test]
    fn retention_keeps_the_tail_and_counts_every_byte() {
        let state = captured(&[b"abcde", b"fgh"], 4);
        assert_eq!(state.retained, b"efgh");
        assert_eq!(state.total, 8);
    }

    /// Reads scale with retention, within bounds a pipe still likes.
    #[test]
    fn the_read_buffer_scales_with_the_cap() {
        assert_eq!(read_buffer_len(1024), 8 * 1024);
        assert_eq!(read_buffer_len(2 * 1024 * 1024), 128 * 1024);
        assert_eq!(read_buffer_len(usize::MAX), 256 * 1024);
    }

    /// Everything downstream reaps with `killpg(child_pid)`, which only
    /// finds anything if the child leads a group of its own. Pin that
    /// here rather than discovering it from a stray `sleep`.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_shell_child_leads_its_own_process_group() {
        let mut child = shell_command("sleep 30", std::path::Path::new("."))
            .spawn()
            .expect("pre_exec must not fail on a healthy fork");
        let pid = i32::try_from(child.id().unwrap()).unwrap();

        // SAFETY: reading the group of a live child of ours.
        let group = unsafe { libc::getpgid(pid) };

        assert_eq!(group, pid, "the child shares ilar's own process group");
        kill_process_group(child.id().unwrap());
        let _ = child.wait().await;
    }

    /// A group id outlives its group, and the kernel hands it out
    /// again. Whoever holds one must be able to ask whether it is still
    /// theirs before signalling it — a service that daemonizes keeps
    /// its group id for the whole session, long enough for that to
    /// matter.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_group_that_has_exited_is_not_signalable() {
        let mut child = shell_command("exit 0", std::path::Path::new("."))
            .spawn()
            .expect("pre_exec must not fail on a healthy fork");
        let pid = child.id().unwrap();
        assert!(process_group_signalable(pid), "a live group answers");

        let _ = child.wait().await;

        assert!(
            !process_group_signalable(pid),
            "a reaped group still claimed its id"
        );
    }

    #[test]
    fn a_single_oversized_chunk_keeps_its_own_tail() {
        let state = captured(&[b"0123456789"], 3);
        assert_eq!(state.retained, b"789");
        assert_eq!(state.total, 10);
    }
}
