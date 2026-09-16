//! Bytes on their way to a vision model.
//!
//! Sniffing, downscaling and PNG re-encoding, shared by everything that
//! hands a provider an image: the TUI's clipboard paste and file drop
//! today, tool results tomorrow. `ImageContent` itself lives with the
//! session model; this is the pipeline that produces one.

use crate::session::ImageContent;
use anyhow::Result;

/// Longest edge providers keep before tiling; larger is waste.
pub const MAX_IMAGE_DIM: usize = 2048;

/// Longest edge an image is stored at on its way in. Vision models
/// see about this much; a full-size PNG of a generated picture is a
/// megabyte or three of base64 that every later request re-uploads.
pub const INGEST_MAX_DIM: usize = 1568;
const JPEG_QUALITY: u8 = 85;

/// Bytes an image file may weigh before it is read at all. Generous
/// on purpose: what is stored is the *shrunk* picture, and a 30 MB
/// scan becomes a couple of hundred kilobytes of JPEG, so refusing it
/// on its compressed size would refuse a perfectly ordinary file for
/// the sake of a decode this pipeline is not going to do. What the
/// bound is really for is the read itself — a drop is somebody's video
/// file as often as it is a picture — and the pixel limit below is
/// what stands between a header's claim and the frame buffer.
pub const MAX_IMAGE_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// The RGBA8 frame buffer one decode may ask for. Not the peak: a
/// PNG that arrives as RGB is held once as it decoded and once
/// widened to RGBA, so a maximal legitimate picture passes through
/// something closer to twice this. It is the number the pixel limit
/// below is derived from, in the units a machine allocates in.
pub const MAX_DECODED_BYTES: usize = 256 * 1024 * 1024;

/// Pixels an image may have before it is refused unread, at four bytes
/// each. Compressed size says nothing about this: a few kilobytes of
/// PNG can declare 40,000 by 40,000 and ask the decoder for six
/// gigabytes, and the decoder will try. Sixty-four megapixels is more
/// than a 6K screenshot or any phone's camera — and it is applied to a
/// JPEG too, which nothing here decodes, because a picture this size
/// is not one to carry into a request either.
pub const MAX_IMAGE_PIXELS: usize = MAX_DECODED_BYTES / 4;

/// Whether an image of this size may be decoded at all.
pub fn within_decode_limits(width: usize, height: usize) -> bool {
    width
        .checked_mul(height)
        .is_some_and(|pixels| pixels <= MAX_IMAGE_PIXELS)
}

/// What stands where an image was once a cutoff dropped it from the
/// request. The text around it still names the file.
pub const IMAGE_ELIDED: &str =
    "[image omitted to keep the request small; read the file again to see it]";

/// How many image bytes a request may carry before older images are
/// dropped from it, and how many of the newest survive the drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageBudget {
    /// Base64 characters, summed over the images still in the request.
    pub max_bytes: usize,
    /// Images kept, newest first, when the cap is crossed.
    pub keep: usize,
}

/// The budget the turn loop applies. Lemonade's router refuses a body
/// of 100 MB; llama.cpp's own server had a cap of the same order.
pub const REQUEST_IMAGE_BUDGET: ImageBudget = ImageBudget {
    max_bytes: 24 * 1024 * 1024,
    keep: 4,
};

/// The canonical index images are dropped before: the latest cutoff
/// the log records, or zero.
pub fn image_cut(events: &[crate::session::SessionEvent]) -> usize {
    events
        .iter()
        .filter_map(|event| match event {
            crate::session::SessionEvent::ImageCutoff { before, .. } => Some(*before),
            _ => None,
        })
        .max()
        .unwrap_or(0)
}

fn images_in(event: &crate::session::SessionEvent) -> &[ImageContent] {
    match event {
        crate::session::SessionEvent::UserMessage { images, .. }
        | crate::session::SessionEvent::ToolResult { images, .. } => images,
        _ => &[],
    }
}

/// Where the next cutoff goes, if the images past the current one
/// outweigh the budget: the index of the event holding the oldest of
/// the `keep` newest images, so everything before it loses its
/// pictures in one rewrite and the prefix then stays put. `None`
/// while the budget holds.
pub fn cutoff_before(
    events: &[crate::session::SessionEvent],
    budget: ImageBudget,
) -> Option<usize> {
    // Pictures a compaction already summarised away do not count.
    let cut = image_cut(events).max(crate::session::compaction_cut(events));
    let carried: usize = events[cut.min(events.len())..]
        .iter()
        .flat_map(|event| images_in(event).iter().map(|image| image.data.len()))
        .sum();
    if carried <= budget.max_bytes {
        return None;
    }
    let mut kept = 0;
    let mut before = events.len();
    for (index, event) in events.iter().enumerate().rev() {
        let here = images_in(event).len();
        if here == 0 {
            continue;
        }
        kept += here;
        before = index;
        if kept >= budget.keep {
            break;
        }
    }
    (before > cut).then_some(before)
}

/// One line per image, naming what the transcript will not show: the
/// kind and the decoded size. This is the tool-result wording; user
/// attachments use [`attachment_markers`]. Both formats sizes the same
/// way, so live rows, restored rows and pasted images agree.
pub fn markers(images: &[ImageContent]) -> String {
    marker_lines(images, "image")
}

/// [`markers`] with the wording a user's own attachment carries.
pub fn attachment_markers(images: &[ImageContent]) -> String {
    marker_lines(images, "image attached")
}

fn marker_lines(images: &[ImageContent], label: &str) -> String {
    images
        .iter()
        .map(|image| {
            let kind = image
                .media_type
                .strip_prefix("image/")
                .unwrap_or(&image.media_type);
            format!(
                "\n[{label}: {kind} · {}]",
                crate::text::format_bytes(image.byte_len() as u64)
            )
        })
        .collect()
}

/// The image formats every provider accepts, by magic number — the
/// extension may lie. One row per format carries both the wire media
/// type and the name the `read` tool shows, so the two cannot drift.
fn sniff(bytes: &[u8]) -> Option<(&'static str, &'static str)> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(("image/png", "PNG"))
    } else if bytes.starts_with(b"\xFF\xD8\xFF") {
        Some(("image/jpeg", "JPEG"))
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some(("image/webp", "WebP"))
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(("image/gif", "GIF"))
    } else {
        None
    }
}

/// Media type by magic numbers — the extension may lie.
pub fn media_type(bytes: &[u8]) -> Option<&'static str> {
    sniff(bytes).map(|(media_type, _)| media_type)
}

/// The same sniff, as the format name a human-readable line uses.
pub fn format_name(bytes: &[u8]) -> Option<&'static str> {
    sniff(bytes).map(|(_, name)| name)
}

/// File bytes → attachment. JPEG, WebP and GIF pass through as
/// themselves (every provider takes them). A PNG is decoded, fitted to
/// [`INGEST_MAX_DIM`], and stored as whichever is smaller: itself, or
/// a JPEG of it when it has no transparency. A PNG that fails to
/// decode passes through untouched.
///
/// `None` for anything that is not an image this program will carry:
/// an unknown format, more bytes than [`MAX_IMAGE_FILE_BYTES`], or a
/// header declaring more pixels than [`MAX_IMAGE_PIXELS`]. A caller
/// with a person to tell asks [`refusal`] which of those it was.
pub fn from_file_bytes(bytes: &[u8]) -> Option<ImageContent> {
    let media_type = media_type(bytes)?;
    if refusal(bytes).is_some() {
        return None;
    }
    if media_type == "image/png" {
        return match shrunk_png(bytes) {
            Shrunk::Image(image) => Some(image),
            // Already as small as it is going to get.
            Shrunk::AsItStands => Some(ImageContent::new(media_type, bytes)),
            Shrunk::TooBig => None,
        };
    }
    Some(ImageContent::new(media_type, bytes))
}

/// Why these bytes will not be carried, in a sentence somebody can
/// read, or `None` when they will be. "Not a supported image" is true
/// of an unknown format and a lie about a 40,000-pixel PNG, which is a
/// format this program knows perfectly well and declines to decode.
pub fn refusal(bytes: &[u8]) -> Option<String> {
    if bytes.len() as u64 > MAX_IMAGE_FILE_BYTES {
        return Some(format!(
            "{} of image, past the {} cap",
            bytes.len(),
            MAX_IMAGE_FILE_BYTES
        ));
    }
    // The size the header claims, read as bytes rather than believed
    // as a picture: it costs nothing, it is known before any decoder
    // has allocated a row, and it is the number a bomb lies about. A
    // format that states no size is bounded by its bytes alone, and by
    // whatever decodes it downstream.
    let (width, height) = declared_size(bytes)?;
    (!within_decode_limits(width as usize, height as usize)).then(|| {
        format!(
            "a picture of {width}×{height}, past the {MAX_IMAGE_PIXELS} pixel cap — too large to \
             decode"
        )
    })
}

/// Width and height as the file's own header states them, for the
/// formats that state them near the front. Believed by nothing: it is
/// what gets *checked* before a decoder is handed the same claim.
pub fn declared_size(bytes: &[u8]) -> Option<(u32, u32)> {
    png_dimensions(bytes).or_else(|| jpeg_dimensions(bytes))
}

/// What [`shrunk_png`] found: a smaller picture, nothing worth
/// changing, or a declared size no decode may be attempted at.
enum Shrunk {
    Image(ImageContent),
    AsItStands,
    TooBig,
}

/// The smaller of the PNG fitted to the ingest size and its JPEG.
fn shrunk_png(bytes: &[u8]) -> Shrunk {
    let (width, height, rgba) = match decode_png(bytes) {
        Decoded::Pixels(width, height, rgba) => (width, height, rgba),
        Decoded::TooBig => return Shrunk::TooBig,
        Decoded::Undecodable => return Shrunk::AsItStands,
    };
    let fitted = downscale_rgba(width, height, &rgba, INGEST_MAX_DIM);
    let (width, height, rgba) = match &fitted {
        Some((w, h, small)) => (*w, *h, small.as_slice()),
        None => (width, height, rgba.as_slice()),
    };
    let opaque = rgba.as_chunks::<4>().0.iter().all(|pixel| pixel[3] == 255);
    let png_bytes = match &fitted {
        Some(_) => match encode_png(width as u32, height as u32, rgba) {
            Ok(encoded) => encoded,
            Err(_) => return Shrunk::AsItStands,
        },
        None => bytes.to_vec(),
    };
    let jpeg_bytes = opaque
        .then(|| encode_jpeg(width as u32, height as u32, rgba).ok())
        .flatten();
    match jpeg_bytes {
        Some(jpeg) if jpeg.len() < png_bytes.len() => {
            Shrunk::Image(ImageContent::new("image/jpeg", &jpeg))
        }
        _ if fitted.is_some() => Shrunk::Image(ImageContent::png(&png_bytes)),
        _ => Shrunk::AsItStands,
    }
}

/// What a PNG's header promised, once the decoder has read it.
enum Decoded {
    Pixels(usize, usize, Vec<u8>),
    /// More pixels than [`MAX_IMAGE_PIXELS`]: refused before the frame
    /// buffer is allocated, which is the whole point of the check.
    TooBig,
    /// Not a PNG this decoder can read, at any size.
    Undecodable,
}

/// A PNG's pixels as RGBA8, when it decodes and is small enough to.
fn decode_png(bytes: &[u8]) -> Decoded {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let Ok(mut reader) = decoder.read_info() else {
        return Decoded::Undecodable;
    };
    // The header is read; nothing the size of the picture has been
    // allocated yet. This is the only moment the check is free.
    let declared = reader.info();
    if !within_decode_limits(declared.width as usize, declared.height as usize) {
        return Decoded::TooBig;
    }
    let Some(size) = reader.output_buffer_size() else {
        return Decoded::Undecodable;
    };
    let mut buf = vec![0; size];
    let Ok(info) = reader.next_frame(&mut buf) else {
        return Decoded::Undecodable;
    };
    buf.truncate(info.buffer_size());
    let (width, height) = (info.width as usize, info.height as usize);
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|&[r, g, b]| [r, g, b, 255])
            .collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&v| [v, v, v, 255]).collect(),
        png::ColorType::GrayscaleAlpha => buf
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|&[v, a]| [v, v, v, a])
            .collect(),
        png::ColorType::Indexed => return Decoded::Undecodable,
    };
    Decoded::Pixels(width, height, rgba)
}

/// RGBA8 rows → JPEG bytes, alpha dropped: for opaque pictures on
/// their way to a model, where a PNG's exactness buys nothing.
pub fn encode_jpeg(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>> {
    let rgb: Vec<u8> = rgba
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|&[r, g, b, _]| [r, g, b])
        .collect();
    let mut out = Vec::new();
    let mut encoder = jpeg_encoder::Encoder::new(&mut out, JPEG_QUALITY);
    // No chroma subsampling: text and thin coloured edges, which a
    // screenshot is made of, smear under 4:2:0.
    encoder.set_sampling_factor(jpeg_encoder::SamplingFactor::F_1_1);
    encoder.encode(
        &rgb,
        u16::try_from(width)?,
        u16::try_from(height)?,
        jpeg_encoder::ColorType::Rgb,
    )?;
    Ok(out)
}

/// Fit RGBA inside `max_dim` on the longest edge with an area-average
/// filter; `None` when it already fits. Providers shrink to ~2048px
/// before tiling anyway, so larger uploads buy nothing but bytes.
pub fn downscale_rgba(
    width: usize,
    height: usize,
    rgba: &[u8],
    max_dim: usize,
) -> Option<(usize, usize, Vec<u8>)> {
    let longest = width.max(height);
    if longest <= max_dim || width == 0 || height == 0 {
        return None;
    }
    let out_width = (width * max_dim / longest).max(1);
    let out_height = (height * max_dim / longest).max(1);
    let mut out = Vec::with_capacity(out_width * out_height * 4);
    for oy in 0..out_height {
        let y0 = oy * height / out_height;
        let y1 = ((oy + 1) * height / out_height).max(y0 + 1);
        for ox in 0..out_width {
            let x0 = ox * width / out_width;
            let x1 = ((ox + 1) * width / out_width).max(x0 + 1);
            let mut sum = [0u64; 4];
            for y in y0..y1 {
                for x in x0..x1 {
                    let pixel = (y * width + x) * 4;
                    for channel in 0..4 {
                        sum[channel] += u64::from(rgba[pixel + channel]);
                    }
                }
            }
            let count = ((y1 - y0) * (x1 - x0)) as u64;
            for channel in sum {
                out.push((channel / count) as u8);
            }
        }
    }
    Some((out_width, out_height, out))
}

/// RGBA8 rows → PNG bytes, for clipboard images headed into a session.
pub fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(rgba)?;
    writer.finish()?;
    Ok(out)
}

/// Roughly what a vision model bills for an image.
///
/// Not its base64 length, which is transport: providers tile an image
/// and charge by pixel area (Anthropic documents width × height / 750,
/// and OpenAI's tiling lands in the same neighbourhood), capped well
/// below what a screenshot's bytes would suggest. Counting the base64
/// instead once read six screenshots as 3.2 M tokens where the provider
/// billed 25 k, which compacted a session one exchange old.
///
/// Only the header is decoded — dimensions live in the first bytes —
/// and an image whose dimensions cannot be read takes the flat estimate
/// rather than its size, because size is the thing that misleads.
pub fn estimated_tokens(image: &ImageContent) -> u64 {
    const PIXELS_PER_TOKEN: u64 = 750;
    const FALLBACK: u64 = 1_600;
    const FLOOR: u64 = 200;
    const CEILING: u64 = 2_400;

    match header_dimensions(&image.data) {
        Some((width, height)) => {
            (u64::from(width) * u64::from(height) / PIXELS_PER_TOKEN).clamp(FLOOR, CEILING)
        }
        None => FALLBACK,
    }
}

/// Width and height from the first bytes of a base64 payload, for the
/// formats that put them there.
fn header_dimensions(data: &str) -> Option<(u32, u32)> {
    use base64::Engine as _;

    // Enough for PNG's IHDR, which comes first, and for a JPEG's
    // frame header, which sits behind its metadata segments; a whole
    // number of quantums so the engine takes the slice as-is.
    let head = data.get(..16_384).unwrap_or(data);
    let head = head.get(..head.len() - head.len() % 4)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(head)
        .ok()?;
    declared_size(&bytes)
}

/// A JPEG's size is in its first start-of-frame segment: height then
/// width, big-endian u16, five bytes past the marker.
pub fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if !bytes.starts_with(b"\xFF\xD8") {
        return None;
    }
    let mut at = 2;
    while at + 4 <= bytes.len() {
        if bytes[at] != 0xFF {
            return None;
        }
        let marker = bytes[at + 1];
        if marker == 0xFF {
            at += 1;
            continue;
        }
        let length = usize::from(u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]));
        if matches!(marker, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF) {
            let height = u32::from(u16::from_be_bytes([
                *bytes.get(at + 5)?,
                *bytes.get(at + 6)?,
            ]));
            let width = u32::from(u16::from_be_bytes([
                *bytes.get(at + 7)?,
                *bytes.get(at + 8)?,
            ]));
            return (width > 0 && height > 0).then_some((width, height));
        }
        at += 2 + length;
    }
    None
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

#[cfg(test)]
mod tests {
    /// A real, complete, tiny PNG whose header says it is `width` by
    /// `height`: a hundred-odd bytes on disk, and an invitation to
    /// allocate however much the header asks for. The shape of a
    /// decode bomb, and a file every decoder will open.
    ///
    /// IHDR is the first chunk and fixed in size, so its fields sit at
    /// known offsets: 8 bytes of signature, 4 of length, 4 of type,
    /// then width and height.
    fn png_claiming(width: u32, height: u32) -> Vec<u8> {
        let mut png = super::encode_png(4, 4, &[0u8; 64]).expect("encodes");
        png[16..20].copy_from_slice(&width.to_be_bytes());
        png[20..24].copy_from_slice(&height.to_be_bytes());
        // The chunk's CRC covers its type and data: bytes 12 to 29.
        let crc = crc32(&png[12..29]);
        png[29..33].copy_from_slice(&crc.to_be_bytes());
        png
    }

    /// PNG chunks carry a CRC-32 and the decoder checks it, so a
    /// header rewritten in place needs a new one — otherwise the
    /// decoder refuses the checksum and proves nothing about the size.
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = u32::MAX;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    /// The header is the only thing that has to be believed, and it is
    /// cheap to lie in: a few dozen bytes asking for six gigabytes of
    /// frame buffer. It is refused before anything is allocated, and
    /// refused outright rather than passed through — an image nothing
    /// here will decode is not one to hand a provider either.
    #[test]
    fn a_png_that_declares_a_machines_worth_of_pixels_is_refused_unread() {
        let bomb = png_claiming(40_000, 40_000);
        assert!(bomb.len() < 200, "the bomb is small, that is the point");
        assert!(super::media_type(&bomb) == Some("image/png"));
        assert!(super::from_file_bytes(&bomb).is_none());
        // And it says which refusal it was: "not a supported image"
        // would be a lie about a PNG.
        let why = super::refusal(&bomb).expect("refused");
        assert!(why.contains("40000×40000"), "{why}");
        assert!(why.contains("too large to decode"), "{why}");
        // The decoder's own guard, which the header check above keeps
        // anything from reaching, still holds on its own: whoever
        // moves that check is caught here rather than by a machine
        // running out of memory.
        assert!(matches!(super::decode_png(&bomb), super::Decoded::TooBig));

        // The limit itself, from both sides.
        assert!(super::within_decode_limits(8192, 8192));
        assert!(!super::within_decode_limits(40_000, 40_000));
        // Nothing overflows on the way to the answer.
        assert!(!super::within_decode_limits(usize::MAX, 2));

        // An ordinary screenshot is untouched by any of this.
        let png = super::encode_png(64, 48, &vec![9u8; 64 * 48 * 4]).expect("encodes");
        assert!(super::from_file_bytes(&png).is_some());
    }

    /// File bytes are bounded in the one place that knows what an
    /// image is, so every door — a drop, a chat attachment, the read
    /// tool — gets the same answer, and says which one it is.
    #[test]
    fn more_bytes_than_the_cap_is_not_an_attachment() {
        let mut png = super::encode_png(4, 4, &[0u8; 64]).expect("encodes");
        assert!(super::from_file_bytes(&png).is_some());
        assert!(super::refusal(&png).is_none());
        png.resize(super::MAX_IMAGE_FILE_BYTES as usize + 1, 0);
        assert!(super::from_file_bytes(&png).is_none());
        assert!(
            super::refusal(&png).is_some_and(|why| why.contains("cap")),
            "the size is why, and it says so"
        );
    }

    /// The bug this replaced: six screenshots estimated to 3.2 M tokens
    /// from their base64 length, against a provider that billed 25 k for
    /// the request that carried them — so the next turn compacted a
    /// conversation one exchange old.
    #[test]
    fn a_screenshot_is_billed_by_its_pixels_not_its_bytes() {
        let pixels = vec![0u8; 2560 * 1440 * 4];
        let png = super::encode_png(2560, 1440, &pixels).expect("encodes");
        let image = crate::session::ImageContent::png(&png);

        let tokens = super::estimated_tokens(&image);
        assert!(
            (2000..=2400).contains(&tokens),
            "a 2560x1440 screenshot costs about (w*h)/750, capped: {tokens}"
        );
        // Six of them are thousands of tokens, not millions.
        assert!(tokens * 6 < 20_000, "{tokens}");

        // An unreadable header takes the flat estimate, never the size.
        let opaque = crate::session::ImageContent::new("image/jpeg", &vec![0u8; 3_000_000]);
        assert_eq!(super::estimated_tokens(&opaque), 1_600);
    }

    use super::*;

    #[test]
    fn image_markers_name_the_kind_and_the_decoded_size() {
        // 12,600 decoded bytes: base64 is 16,800 chars, byte_len halves
        // back to 12,600, which rounds to one decimal like everywhere else.
        let png = ImageContent::png(&vec![0u8; 12_600]);
        let jpeg = ImageContent::new("image/jpeg", &[0u8; 900]);

        assert_eq!(markers(&[]), "");
        assert_eq!(
            markers(std::slice::from_ref(&png)),
            "\n[image: png · 12.3 KiB]"
        );
        // One line per image, in order, whatever the kind.
        assert_eq!(
            markers(&[png.clone(), jpeg]),
            "\n[image: png · 12.3 KiB]\n[image: jpeg · 900 B]"
        );

        // Same size formatting, the user-attachment wording.
        assert_eq!(
            attachment_markers(&[png]),
            "\n[image attached: png · 12.3 KiB]"
        );

        // A media type without the usual prefix is shown as-is.
        assert_eq!(
            markers(&[ImageContent::new("weird", &[0u8; 3])]),
            "\n[image: weird · 3 B]"
        );
    }

    #[test]
    fn image_bytes_are_sniffed_and_oversized_pngs_downscale_on_the_way_in() {
        // Magic numbers, not extensions.
        assert_eq!(media_type(b"\x89PNG\r\n\x1a\nrest"), Some("image/png"));
        assert_eq!(media_type(b"\xFF\xD8\xFF\xE0rest"), Some("image/jpeg"));
        assert_eq!(
            media_type(b"RIFF\x00\x00\x00\x00WEBPVP8 "),
            Some("image/webp")
        );
        assert_eq!(media_type(b"GIF89arest"), Some("image/gif"));
        assert_eq!(media_type(b"plain text"), None);

        // A jpeg passes through byte-identically with its own type.
        let jpeg = b"\xFF\xD8\xFF\xE0fake jpeg body";
        let content = from_file_bytes(jpeg).unwrap();
        assert_eq!(content, ImageContent::new("image/jpeg", jpeg));

        // A small png passes through untouched: its JPEG would be bigger.
        let small = encode_png(4, 4, &[9u8; 4 * 4 * 4]).unwrap();
        let content = from_file_bytes(&small).unwrap();
        assert_eq!(content, ImageContent::png(&small));

        // An oversized opaque photo-like png is fitted to the ingest
        // size and stored as a JPEG, since that is smaller. (A flat
        // colour would stay a PNG: smaller wins.)
        let mut seed = 7u32;
        let noisy: Vec<u8> = (0..3000 * 1000 * 4)
            .map(|i| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                if i % 4 == 3 { 255 } else { (seed >> 24) as u8 }
            })
            .collect();
        let big = encode_png(3000, 1000, &noisy).unwrap();
        let content = from_file_bytes(&big).unwrap();
        assert_eq!(content.media_type, "image/jpeg");
        assert_eq!(header_dimensions(&content.data), Some((1568, 522)));
        assert!(content.byte_len() < big.len() / 4, "{}", content.byte_len());

        // A transparent one stays a PNG, fitted.
        let mut see_through = noisy.clone();
        see_through[3] = 0;
        let big = encode_png(3000, 1000, &see_through).unwrap();
        let content = from_file_bytes(&big).unwrap();
        assert_eq!(content.media_type, "image/png");
        assert_eq!(header_dimensions(&content.data), Some((1568, 522)));
    }

    #[test]
    fn a_cutoff_lands_before_the_newest_few_images_once_the_budget_is_crossed() {
        use crate::session::SessionEvent;
        let image = |bytes: usize| ImageContent::new("image/png", &vec![0u8; bytes]);
        let user = |images: Vec<ImageContent>| SessionEvent::UserMessage {
            id: crate::session::new_id(),
            text: "look".into(),
            images,
            ts: chrono::Utc::now(),
        };
        let budget = ImageBudget {
            max_bytes: 1_000,
            keep: 2,
        };
        // Four images of 300 bytes each: 1,600 characters of base64.
        let events = vec![
            user(vec![image(300)]),
            user(vec![]),
            user(vec![image(300)]),
            user(vec![image(300)]),
            user(vec![image(300)]),
        ];
        // The newest two live in events 3 and 4: the cut goes before 3.
        assert_eq!(cutoff_before(&events, budget), Some(3));
        let mut with_cut = events.clone();
        with_cut.push(SessionEvent::ImageCutoff {
            id: crate::session::new_id(),
            before: 3,
            ts: chrono::Utc::now(),
        });
        assert_eq!(image_cut(&with_cut), 3);
        // Two images past the cut fit the budget: nothing more to do.
        assert_eq!(cutoff_before(&with_cut, budget), None);
        // Under budget from the start: nothing either.
        assert_eq!(cutoff_before(&events[..2], budget), None);
    }

    #[test]
    fn oversized_images_downscale_to_fit_and_small_ones_pass_through() {
        // Already fits: untouched.
        assert!(downscale_rgba(200, 100, &[0u8; 200 * 100 * 4], 2048).is_none());

        // Longest edge maps to the cap, aspect preserved.
        let (width, height, out) =
            downscale_rgba(4000, 2000, &vec![128u8; 4000 * 2000 * 4], 2048).unwrap();
        assert_eq!((width, height), (2048, 1024));
        assert_eq!(out.len(), 2048 * 1024 * 4);
        // Constant color survives averaging exactly.
        assert!(out.iter().all(|&byte| byte == 128));

        // Distinct halves average within themselves, not across.
        let src = [
            255, 0, 0, 255, 255, 0, 0, 255, // red, red
            0, 0, 255, 255, 0, 0, 255, 255, // blue, blue
        ];
        let (width, height, out) = downscale_rgba(4, 1, &src, 2).unwrap();
        assert_eq!((width, height), (2, 1));
        assert_eq!(&out[..4], &[255, 0, 0, 255]);
        assert_eq!(&out[4..], &[0, 0, 255, 255]);
    }
}
