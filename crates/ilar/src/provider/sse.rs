//! Incremental SSE parser: feed bytes, get complete `data:` payloads.
//!
//! Buffers raw bytes so multi-byte UTF-8 split across chunk boundaries is
//! never corrupted; blocks are only converted to text once complete
//! (block boundaries are ASCII `\n`, so a complete block is valid UTF-8
//! iff the stream is).

pub struct SseParser {
    buffer: Vec<u8>,
}

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
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(pos) = self.next_event_boundary() {
            let block: Vec<u8> = self.buffer.drain(..pos).collect();
            self.trim_boundary();
            if let Some(data) = parse_block(&block) {
                out.push(data);
            }
        }
        out
    }

    fn next_event_boundary(&self) -> Option<usize> {
        let lf = find_subslice(&self.buffer, b"\n\n").map(|i| i + 1);
        let crlf = find_subslice(&self.buffer, b"\r\n\r\n").map(|i| i + 2);
        match (lf, crlf) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn trim_boundary(&mut self) {
        if self.buffer.starts_with(b"\r\n") {
            self.buffer.drain(..2);
        } else if self.buffer.starts_with(b"\n") {
            self.buffer.drain(..1);
        }
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Extract the joined `data:` lines of one event block; None for blocks
/// with no data (comments, bare event: lines).
fn parse_block(block: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(block);
    let mut data: Vec<&str> = Vec::new();
    for line in text.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(rest) = line.strip_prefix("data:") {
            data.push(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    if data.is_empty() {
        None
    } else {
        Some(data.join("\n"))
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
            events.extend(parser.feed(&[b]));
        }
        assert_eq!(events, vec!["{\"a\":1}", "{\"b\":\n2}"]);
    }

    #[test]
    fn handles_crlf_and_comments() {
        let mut parser = SseParser::new();
        let raw = ": keepalive\r\n\r\ndata: hi\r\n\r\n";
        let events = parser.feed(raw.as_bytes());
        assert_eq!(events, vec!["hi"]);
    }

    #[test]
    fn incomplete_tail_is_buffered() {
        let mut parser = SseParser::new();
        assert!(parser.feed(b"data: {\"x\"").is_empty());
        assert_eq!(parser.feed(b":1}\n\n"), vec!["{\"x\":1}"]);
    }

    #[test]
    fn multibyte_utf8_split_across_chunks_survives() {
        let _parser = SseParser::new();
        let payload = "data: {\"delta\":\"日本語\"}\n\n";
        let bytes = payload.as_bytes();
        // Split at every byte offset inside the multibyte chars.
        for split in [4usize, 13, 14, 15, 16] {
            let mut parser = SseParser::new();
            let mut events = parser.feed(&bytes[..split]);
            events.extend(parser.feed(&bytes[split..]));
            assert_eq!(events, vec!["{\"delta\":\"日本語\"}"], "split at {split}");
        }
    }
}
