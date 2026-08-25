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

/// Last `keep` bytes of `bytes`, starting on a UTF-8 boundary so a tail
/// cut mid-codepoint does not open with a replacement character.
pub(crate) fn tail(bytes: &[u8], keep: usize) -> &[u8] {
    if keep >= bytes.len() {
        return bytes;
    }
    let mut start = bytes.len() - keep;
    while start < bytes.len() && bytes[start] & 0b1100_0000 == 0b1000_0000 {
        start += 1;
    }
    &bytes[start..]
}

/// Read `reader` to EOF into `captured`, keeping at most `cap` bytes.
pub(crate) async fn drain<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    cap: usize,
    captured: Arc<Mutex<Captured>>,
) {
    use tokio::io::AsyncReadExt;

    let mut buffer = [0u8; 8192];
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
/// process group so descendants can be killed as a unit.
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
    command.process_group(0);
    command
}

/// SIGKILL the group led by a [`shell_command`] child: it starts a fresh
/// process group whose id equals its pid.
#[cfg(unix)]
pub(crate) fn kill_process_group(pid: u32) {
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

    #[test]
    fn a_single_oversized_chunk_keeps_its_own_tail() {
        let state = captured(&[b"0123456789"], 3);
        assert_eq!(state.retained, b"789");
        assert_eq!(state.total, 10);
    }

    #[test]
    fn tail_starts_on_a_utf8_boundary() {
        let bytes = "aé".as_bytes(); // 61 c3 a9
        assert_eq!(tail(bytes, 3), bytes);
        assert_eq!(tail(bytes, 2), "é".as_bytes());
        // Cutting inside the codepoint skips its stray continuation byte.
        assert_eq!(tail(bytes, 1), b"");
    }
}
