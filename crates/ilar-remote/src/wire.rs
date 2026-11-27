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

/// Bounds a line from the device: a prompt with a few photos, which the
/// phone scales down before it sends them.
pub const MAX_FROM_DEVICE: usize = 16 * 1024 * 1024;
/// Bounds a line from the host: a snapshot carries a whole session.
pub const MAX_TO_DEVICE: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToDevice {
    /// The whole picture: on connect, and whenever the log is rewound or
    /// resynced (cheaper to resend than to explain).
    Snapshot {
        /// The whole log as one page, in the shape `ilar serve` answers
        /// `/api/sessions/{id}` with (`events`, `line`, `session`,
        /// `usage`, ...), so the same renderer folds it.
        page: Value,
        /// The running step's deltas so far (`LiveDelta`s).
        live: Vec<Value>,
        busy: bool,
        prompt: Option<Prompt>,
    },
    /// One event appended to the log at physical line `line`; a
    /// client at or past it already has the event.
    Append {
        line: usize,
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
    /// The model menu: what the session runs on, and what it could.
    /// Sent after the snapshot and again on every change.
    Models {
        current: String,
        /// The thinking level; `None` is the provider's default.
        variant: Option<String>,
        models: Vec<ModelChoice>,
    },
    /// The answer to `FromDevice::Image`; `None` when there is no such
    /// image.
    Image {
        event_id: String,
        n: usize,
        image: Option<Image>,
    },
}

/// An image as the session stores it (`ilar::session::ImageContent`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Image {
    /// e.g. "image/jpeg".
    pub media_type: String,
    /// Base64 bytes.
    pub data: String,
}

/// A model the session may switch to, with its thinking levels.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelChoice {
    /// `provider/model`, as the TUI's picker names it.
    pub id: String,
    pub name: String,
    /// Whether it takes images, so the phone offers photos only then.
    pub vision: bool,
    pub variants: Vec<Choice>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    pub id: String,
    pub name: String,
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
    /// if typed into the TUI, with the images attached to it.
    Prompt {
        text: String,
        #[serde(default)]
        images: Vec<Image>,
    },
    /// The bytes of image `n` of event `event_id` (the projection sends
    /// only a descriptor).
    Image {
        event_id: String,
        n: usize,
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
    /// Switch the model and thinking level (`None`: the provider's
    /// default), as the TUI's picker does: between turns only.
    Model {
        model: String,
        variant: Option<String>,
    },
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
    fn the_model_menu_and_a_choice_have_a_stable_json_shape() {
        let models = ToDevice::Models {
            current: "zai/glm-5.2".into(),
            variant: Some("high".into()),
            models: vec![ModelChoice {
                id: "zai/glm-5.2".into(),
                name: "GLM 5.2".into(),
                vision: false,
                variants: vec![Choice {
                    id: "high".into(),
                    name: "High".into(),
                }],
            }],
        };
        assert_eq!(
            String::from_utf8(encode(&models)).unwrap(),
            "{\"type\":\"models\",\"current\":\"zai/glm-5.2\",\"variant\":\"high\",\"models\":[{\"id\":\"zai/glm-5.2\",\"name\":\"GLM 5.2\",\"vision\":false,\"variants\":[{\"id\":\"high\",\"name\":\"High\"}]}]}\n"
        );
        let choice: FromDevice =
            serde_json::from_str("{\"type\":\"model\",\"model\":\"a/b\",\"variant\":null}")
                .unwrap();
        assert_eq!(
            choice,
            FromDevice::Model {
                model: "a/b".into(),
                variant: None
            }
        );
    }

    #[test]
    fn a_prompt_carries_images_and_an_image_is_asked_for_by_event() {
        let bare: FromDevice =
            serde_json::from_str("{\"type\":\"prompt\",\"text\":\"hi\"}").unwrap();
        assert_eq!(
            bare,
            FromDevice::Prompt {
                text: "hi".into(),
                images: Vec::new()
            },
            "a prompt without images is still a prompt"
        );
        let photo = Image {
            media_type: "image/jpeg".into(),
            data: "AAAA".into(),
        };
        let with: FromDevice = serde_json::from_str(
            "{\"type\":\"prompt\",\"text\":\"\",\"images\":[{\"media_type\":\"image/jpeg\",\"data\":\"AAAA\"}]}",
        )
        .unwrap();
        assert_eq!(
            with,
            FromDevice::Prompt {
                text: String::new(),
                images: vec![photo.clone()]
            }
        );
        let ask: FromDevice =
            serde_json::from_str("{\"type\":\"image\",\"event_id\":\"e1\",\"n\":0}").unwrap();
        assert_eq!(
            ask,
            FromDevice::Image {
                event_id: "e1".into(),
                n: 0
            }
        );
        assert_eq!(
            String::from_utf8(encode(&ToDevice::Image {
                event_id: "e1".into(),
                n: 0,
                image: Some(photo),
            }))
            .unwrap(),
            "{\"type\":\"image\",\"event_id\":\"e1\",\"n\":0,\"image\":{\"media_type\":\"image/jpeg\",\"data\":\"AAAA\"}}\n"
        );
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
