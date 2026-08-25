//! Binary sniffing for `read`: raw bytes in the transcript are useless to
//! the model and tokenize at ~1.2 bytes/token, so a binary file is
//! described in one line instead of being dumped.
//!
//! Deliberately conservative — source code, logs and ANSI-colored terminal
//! captures must keep reading as text.

/// How much of the file head the heuristics look at.
pub const SNIFF_BYTES: usize = 8 * 1024;

/// A control character run has to make up more than this share of the head
/// before a valid-UTF-8, NUL-free file is called binary.
const CONTROL_PERCENT: usize = 5;

/// Below this, control characters are a stray escape, not a file format.
const MIN_CONTROLS: usize = 4;

/// Closing hint when the model gets none of the file's bytes.
const NO_RETRY_HINT: &str = "cannot be read as text, do not retry with offset/limit";

/// Closing hint when the result carries the image itself.
const IMAGE_FOLLOWS_HINT: &str = "the image itself follows";

/// `Some(one-line description)` when `head` — the first [`SNIFF_BYTES`] of
/// a `total_bytes`-long file — is binary; `None` when it reads as text.
pub fn describe(display_path: &str, head: &[u8], total_bytes: u64) -> Option<String> {
    let kind =
        image_kind(head).or_else(|| looks_binary(head).then(|| "binary data".to_string()))?;
    Some(line(display_path, &kind, total_bytes, NO_RETRY_HINT))
}

/// The [`describe`] line for an image whose bytes the result actually
/// carries, so the hint points at the attachment instead of telling the
/// model not to retry. `None` when `head` is not an image at all — which
/// is also how a caller tells an image apart from generic binary data.
pub(crate) fn describe_attached_image(
    display_path: &str,
    head: &[u8],
    total_bytes: u64,
) -> Option<String> {
    Some(line(
        display_path,
        &image_kind(head)?,
        total_bytes,
        IMAGE_FOLLOWS_HINT,
    ))
}

fn line(display_path: &str, kind: &str, total_bytes: u64, hint: &str) -> String {
    format!("(binary file: {display_path} — {kind}, {total_bytes} bytes; {hint})")
}

/// Magic-byte image formats, with dimensions where they are a cheap
/// read. The magic numbers themselves are `crate::image`'s table: what
/// this tool calls an image and what the vision pipeline will accept
/// have to be the same set, or the read tool would name a format it then
/// declines to attach.
fn image_kind(head: &[u8]) -> Option<String> {
    let format = crate::image::format_name(head)?;
    Some(match png_dimensions(head) {
        Some((width, height)) => format!("{format} image, {width}x{height}"),
        None => format!("{format} image"),
    })
}

/// PNG puts IHDR first: width and height are big-endian u32 at 16..24.
fn png_dimensions(head: &[u8]) -> Option<(u32, u32)> {
    if head.len() < 24 || !head.starts_with(b"\x89PNG\r\n\x1a\n") || &head[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(head[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(head[20..24].try_into().ok()?);
    (width > 0 && height > 0).then_some((width, height))
}

/// NUL byte, invalid UTF-8, or a dense run of control characters. TAB, LF,
/// CR, FF and ESC are text: ANSI-colored logs must not trip this.
fn looks_binary(head: &[u8]) -> bool {
    if head.is_empty() {
        return false;
    }
    if head.contains(&0) || !valid_utf8_prefix(head) {
        return true;
    }
    let controls = head.iter().filter(|byte| is_binary_control(**byte)).count();
    controls >= MIN_CONTROLS && controls * 100 > head.len() * CONTROL_PERCENT
}

fn is_binary_control(byte: u8) -> bool {
    matches!(byte, 0x00..=0x08 | 0x0b | 0x0e..=0x1a | 0x1c..=0x1f | 0x7f)
}

/// A multi-byte character cut in half by the sniff window is not evidence
/// of binary content, so only a genuine decode error counts.
fn valid_utf8_prefix(head: &[u8]) -> bool {
    match std::str::from_utf8(head) {
        Ok(_) => true,
        Err(error) => error.error_len().is_none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&13_u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
        bytes
    }

    #[test]
    fn images_report_format_and_png_dimensions() {
        let described = describe("a.png", &png(1920, 1080), 4096).unwrap();
        assert!(described.contains("PNG image, 1920x1080"), "{described}");
        assert!(described.contains("4096 bytes"), "{described}");
        assert_eq!(
            image_kind(b"\xFF\xD8\xFF\xE0junk").as_deref(),
            Some("JPEG image")
        );
        assert_eq!(
            image_kind(b"RIFF\x00\x00\x00\x00WEBPVP8 ").as_deref(),
            Some("WebP image")
        );
        assert_eq!(
            image_kind(b"GIF89a\x10\x00\x10\x00").as_deref(),
            Some("GIF image")
        );
    }

    #[test]
    fn truncated_png_header_still_names_the_format() {
        let kind = image_kind(b"\x89PNG\r\n\x1a\n\x00\x00").unwrap();
        assert_eq!(kind, "PNG image");
    }

    #[test]
    fn text_shapes_are_not_binary() {
        for text in [
            "fn main() { println!(\"héllo — 世界 🌍\"); }\n",
            "\u{1b}[32mINFO\u{1b}[0m started\n\u{1b}[31mERROR\u{1b}[0m failed\n",
            "col\tcol\r\nrow\trow\r\n",
            "",
            "\u{1b}[1;31m\u{1b}[0m\u{1b}[1;31m\u{1b}[0m",
        ] {
            assert!(!looks_binary(text.as_bytes()), "{text:?}");
            assert!(describe("f.txt", text.as_bytes(), 10).is_none(), "{text:?}");
        }
    }

    #[test]
    fn nul_and_invalid_utf8_and_control_density_are_binary() {
        assert!(looks_binary(b"plain text with a \x00 in it"));
        assert!(looks_binary(b"caf\xC3\x28 invalid continuation"));
        assert!(looks_binary(&[0x01, 0x02, 0x03, 0x04, 0x05, b'a', b'b']));
    }

    #[test]
    fn a_stray_control_character_keeps_the_file_text() {
        let mut text = "log line without controls\n".repeat(4);
        text.push('\u{7}');
        assert!(!looks_binary(text.as_bytes()));
    }

    #[test]
    fn a_truncated_multibyte_char_at_the_window_edge_is_text() {
        let mut head = "字".repeat(8).into_bytes();
        head.truncate(head.len() - 1);
        assert!(!looks_binary(&head));
    }

    #[test]
    fn only_images_get_the_attached_image_hint() {
        let described = describe_attached_image("a.png", &png(48, 32), 289).unwrap();
        assert_eq!(
            described,
            "(binary file: a.png — PNG image, 48x32, 289 bytes; the image itself follows)"
        );
        // Generic binary has no image to attach.
        assert!(describe_attached_image("blob.bin", &[0_u8; 64], 64).is_none());
        // …and text is not binary at all, on either entry point.
        assert!(describe_attached_image("f.txt", b"plain text\n", 11).is_none());
    }

    #[test]
    fn generic_binary_description_names_size_and_the_no_retry_hint() {
        let described = describe("blob.bin", &[0_u8; 64], 64).unwrap();
        assert!(described.contains("binary data, 64 bytes"), "{described}");
        assert!(described.contains("cannot be read as text"), "{described}");
        assert!(
            described.contains("do not retry with offset/limit"),
            "{described}"
        );
        assert_eq!(described.lines().count(), 1);
    }
}
