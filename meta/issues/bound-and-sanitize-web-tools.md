# Bound and sanitize web tools

## Summary

Web tools have no total timeout, buffer entire bodies before limits, and use unsafe byte offsets for case-insensitive HTML stripping.

## Requirements

- Apply connect and total request timeouts.
- Stream responses with byte limits instead of buffering without bounds.
- Use byte-safe HTML processing or a real parser.
- Preserve block boundaries in converted text.
- Clamp search result limits and bound backend JSON responses.
- Block loopback, private, link-local, and metadata-network targets, including redirect destinations.

## Acceptance Criteria

- Slow and oversized responses fail promptly with bounded memory.
- Unicode before script/style tags cannot panic.
- Adjacent HTML blocks remain separated in output.
- Search limits stay within a documented range.
- SSRF-blocked targets fail before response content is read.
