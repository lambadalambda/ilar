//! The session protocol on the paired stream: one JSON value per line,
//! each way. The first line each way is the handshake (`Hello`, then
//! `Welcome`); everything after is [`ToDevice`] / [`FromDevice`].
//!
//! Event payloads are `serde_json::Value`: they are the session log's
//! web projection (`ilar-tui/src/web/view.rs`), and this crate stays
//! free of the core so the browser can build it.

use anyhow::{Context, Result, bail};
use iroh::endpoint::{RecvStream, SendStream};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Bounds a line from the device: commands are small.
pub const MAX_FROM_DEVICE: usize = 256 * 1024;
/// Bounds a line from the host: a snapshot carries a whole session.
pub const MAX_TO_DEVICE: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToDevice {
    /// The whole picture: on connect, and whenever the log is rewound or
    /// resynced (cheaper to resend than to explain).
    Snapshot {
        /// Projected log events, oldest first.
        events: Vec<Value>,
        /// The running step's deltas so far (`LiveDelta`s).
        live: Vec<Value>,
        busy: bool,
        prompt: Option<Prompt>,
    },
    /// One event appended to the log.
    Append {
        event: Value,
    },
    /// A streaming delta of the running step, or `{"type":"reset"}`
    /// when the step committed or the turn ended.
    Live {
        delta: Value,
    },
    Busy {
        busy: bool,
    },
    /// The open prompt changed; `None` when it was answered or withdrawn.
    Prompt {
        prompt: Option<Prompt>,
    },
    /// Something the device's last command could not do.
    Notice {
        text: String,
    },
}

/// What the session waits on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Prompt {
    /// A tool wants a secret or a command approved.
    Grant {
        tool: String,
        secret: String,
        description: String,
        detail: String,
        agent: Option<String>,
    },
    /// The question tool: `request` is an `ilar::question::QuestionRequest`.
    Question { request: Value },
    /// Something only the terminal may answer (a password).
    Terminal { detail: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromDevice {
    /// Typed text: a new turn, a steer or a queued message, exactly as
    /// if typed into the TUI.
    Prompt {
        text: String,
    },
    /// `secret` names the prompt answered (from `Prompt::Grant`), so a
    /// late answer cannot land on a newer prompt. `None` refuses.
    Grant {
        secret: String,
        answer: Option<GrantAnswer>,
    },
    /// An `ilar::question::QuestionResponse`.
    Question {
        response: Value,
    },
    Cancel,
}

/// Mirrors `ilar::secrets::Grant`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantAnswer {
    Once,
    Session,
    Always,
}

pub fn encode<T: Serialize>(message: &T) -> Vec<u8> {
    let mut line = serde_json::to_vec(message).expect("wire messages serialize");
    line.push(b'\n');
    line
}

pub async fn write<T: Serialize>(send: &mut SendStream, message: &T) -> Result<()> {
    send.write_all(&encode(message)).await?;
    Ok(())
}

/// Splits a byte stream into lines. [`Reader`] drains every whole line
/// before it pushes more, so an unfinished line is held to about `max`
/// plus one chunk.
#[derive(Debug)]
pub(crate) struct Lines {
    buf: Vec<u8>,
    /// Bytes already searched for a newline, so a snapshot arriving in
    /// many chunks is scanned once, not once per chunk.
    scanned: usize,
    max: usize,
}

impl Lines {
    pub(crate) fn new(max: usize) -> Self {
        Self {
            buf: Vec::new(),
            scanned: 0,
            max,
        }
    }

    /// The next whole line already buffered, without its newline.
    pub(crate) fn next_line(&mut self) -> Option<Vec<u8>> {
        let Some(offset) = self.buf[self.scanned..].iter().position(|&b| b == b'\n') else {
            self.scanned = self.buf.len();
            return None;
        };
        let end = self.scanned + offset;
        let mut line: Vec<u8> = self.buf.drain(..=end).collect();
        line.pop();
        self.scanned = 0;
        Some(line)
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<()> {
        self.buf.extend_from_slice(bytes);
        if self.scanned > self.max {
            bail!("a line longer than {} bytes", self.max);
        }
        Ok(())
    }
}

/// Reads one message per line from a stream.
pub struct Reader {
    recv: RecvStream,
    lines: Lines,
}

impl Reader {
    pub fn new(recv: RecvStream, max: usize) -> Self {
        Self {
            recv,
            lines: Lines::new(max),
        }
    }

    /// `None` at the end of the stream.
    pub async fn next<T: serde::de::DeserializeOwned>(&mut self) -> Result<Option<T>> {
        loop {
            if let Some(line) = self.lines.next_line() {
                return serde_json::from_slice(&line)
                    .map(Some)
                    .context("a malformed line");
            }
            let mut chunk = [0u8; 16 * 1024];
            match self.recv.read(&mut chunk).await? {
                Some(n) => self.lines.push(&chunk[..n])?,
                None => return Ok(None),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_split_across_chunks_and_keep_order() {
        let mut lines = Lines::new(64);
        lines.push(b"{\"a\":1}\n{\"b\"").unwrap();
        assert_eq!(lines.next_line().unwrap(), b"{\"a\":1}");
        assert_eq!(lines.next_line(), None);
        lines.push(b":2}\n").unwrap();
        assert_eq!(lines.next_line().unwrap(), b"{\"b\":2}");
    }

    #[test]
    fn an_endless_line_is_refused() {
        let mut lines = Lines::new(8);
        lines.push(b"0123456789").unwrap();
        assert_eq!(lines.next_line(), None);
        assert!(lines.push(b"x").is_err());
    }

    #[test]
    fn messages_have_a_stable_json_shape() {
        let line = encode(&FromDevice::Grant {
            secret: "GH_TOKEN".into(),
            answer: Some(GrantAnswer::Once),
        });
        assert_eq!(
            line,
            b"{\"type\":\"grant\",\"secret\":\"GH_TOKEN\",\"answer\":\"once\"}\n"
        );
        let prompt = ToDevice::Prompt {
            prompt: Some(Prompt::Terminal {
                detail: "sudo".into(),
            }),
        };
        let back: ToDevice = serde_json::from_slice(&encode(&prompt)).unwrap();
        assert_eq!(back, prompt);
    }
}
