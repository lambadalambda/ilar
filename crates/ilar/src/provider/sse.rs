//! Incremental SSE parser: feed bytes, get complete `data:` payloads.
//!
//! Buffers raw bytes so multi-byte UTF-8 split across chunk boundaries is
//! never corrupted; blocks are only converted to text once complete
//! (block boundaries are ASCII `\n`, so a complete block is valid UTF-8
//! iff the stream is).

pub struct SseParser {
    buffer: Vec<u8>,
}

pub(crate) const MAX_SSE_EVENT_BYTES: usize = 1024 * 1024;

impl Default for SseParser {
    fn default() -> Self {
        Self::new()
    }
}

impl SseParser {
    pub fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    /// Feed raw bytes; returns data payloads of complete events.
    pub fn feed(&mut self, bytes: &[u8]) -> anyhow::Result<Vec<String>> {
        let mut out = Vec::new();
        for byte in bytes {
            self.buffer.push(*byte);
            if self.buffer.len() > MAX_SSE_EVENT_BYTES {
                anyhow::bail!("SSE event exceeds {MAX_SSE_EVENT_BYTES} bytes");
            }
            let boundary = if self.buffer.ends_with(b"\r\n\r\n") {
                Some(4)
            } else if self.buffer.ends_with(b"\r\n\n") || self.buffer.ends_with(b"\n\r\n") {
                Some(3)
            } else if self.buffer.ends_with(b"\n\n") || self.buffer.ends_with(b"\r\r") {
                Some(2)
            } else {
                None
            };
            if let Some(boundary) = boundary {
                self.buffer.truncate(self.buffer.len() - boundary);
                if let Some(data) = parse_block(&self.buffer)? {
                    out.push(data);
                }
                self.buffer.clear();
            }
        }
        Ok(out)
    }

    pub fn finish(&mut self) -> anyhow::Result<()> {
        if self.buffer.is_empty() {
            self.buffer.clear();
            Ok(())
        } else {
            anyhow::bail!("SSE stream ended with an unterminated event")
        }
    }
}

/// Extract the joined `data:` lines of one event block; None for blocks
/// with no data (comments, bare event: lines).
fn parse_block(block: &[u8]) -> anyhow::Result<Option<String>> {
    let text =
        std::str::from_utf8(block).map_err(|_| anyhow::anyhow!("invalid UTF-8 in SSE event"))?;
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut data: Vec<&str> = Vec::new();
    for line in normalized.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            data.push(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    if data.is_empty() {
        Ok(None)
    } else {
        Ok(Some(data.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_events_across_chunk_boundaries() {
        let mut parser = SseParser::new();
        // Multi-line data joins with \n per the SSE spec.
        let raw = "event: foo\ndata: {\"a\":1}\n\nevent: bar\ndata: {\"b\":\ndata: 2}\n\n";
        // Feed byte-by-byte to prove boundary handling.
        let mut events = Vec::new();
        for b in raw.bytes() {
            events.extend(parser.feed(&[b]).unwrap());
        }
        assert_eq!(events, vec!["{\"a\":1}", "{\"b\":\n2}"]);
    }

    #[test]
    fn handles_crlf_and_comments() {
        let mut parser = SseParser::new();
        let raw = ": keepalive\r\n\r\ndata: hi\r\n\r\n";
        let events = parser.feed(raw.as_bytes()).unwrap();
        assert_eq!(events, vec!["hi"]);
    }

    #[test]
    fn handles_mixed_and_cr_event_boundaries() {
        let mut parser = SseParser::new();
        let events = parser
            .feed(b"data: one\r\ndata: two\r\rdata: three\n\r\n")
            .unwrap();
        assert_eq!(events, vec!["one\ntwo", "three"]);

        let mut bare_cr = SseParser::new();
        assert_eq!(
            bare_cr.feed(b"data: one\rdata: two\r\r").unwrap(),
            vec!["one\ntwo"]
        );
    }

    #[test]
    fn incomplete_tail_is_buffered() {
        let mut parser = SseParser::new();
        assert!(parser.feed(b"data: {\"x\"").unwrap().is_empty());
        assert_eq!(parser.feed(b":1}\n\n").unwrap(), vec!["{\"x\":1}"]);
    }

    #[test]
    fn multibyte_utf8_split_across_chunks_survives() {
        let _parser = SseParser::new();
        let payload = "data: {\"delta\":\"日本語\"}\n\n";
        let bytes = payload.as_bytes();
        // Split at every byte offset inside the multibyte chars.
        for split in [4usize, 13, 14, 15, 16] {
            let mut parser = SseParser::new();
            let mut events = parser.feed(&bytes[..split]).unwrap();
            events.extend(parser.feed(&bytes[split..]).unwrap());
            assert_eq!(events, vec!["{\"delta\":\"日本語\"}"], "split at {split}");
        }
    }

    #[test]
    fn rejects_invalid_utf8() {
        let mut parser = SseParser::new();
        assert!(parser.feed(b"data: \xff\n\n").is_err());
    }

    #[test]
    fn rejects_oversized_complete_and_incomplete_events() {
        let mut incomplete = SseParser::new();
        assert!(
            incomplete
                .feed(&vec![b'x'; MAX_SSE_EVENT_BYTES + 1])
                .is_err()
        );

        let mut complete = SseParser::new();
        let mut event = b"data: ".to_vec();
        event.extend(vec![b'x'; MAX_SSE_EVENT_BYTES]);
        event.extend_from_slice(b"\n\n");
        assert!(complete.feed(&event).is_err());
    }

    #[test]
    fn finish_rejects_unterminated_data() {
        let mut parser = SseParser::new();
        parser.feed(b"data: partial").unwrap();
        assert!(parser.finish().is_err());

        let mut whitespace = SseParser::new();
        whitespace.feed(b" \t").unwrap();
        assert!(whitespace.finish().is_err());
    }
}
