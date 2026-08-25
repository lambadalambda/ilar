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

/// Media type by magic numbers — the extension may lie.
pub fn media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xFF\xD8\xFF") {
        Some("image/jpeg")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else {
        None
    }
}

/// File bytes → attachment. Formats pass through as themselves (every
/// provider takes png/jpeg/webp/gif); only an oversized PNG — the
/// retina-screenshot case — is decoded, downscaled and re-encoded.
pub fn from_file_bytes(bytes: &[u8]) -> Option<ImageContent> {
    let media_type = media_type(bytes)?;
    if media_type == "image/png"
        && let Some(downscaled) = downscaled_png(bytes)
    {
        return Some(downscaled);
    }
    Some(ImageContent::new(media_type, bytes))
}

/// `Some` only when the PNG decodes cleanly and needed shrinking;
/// anything else falls back to the original bytes.
fn downscaled_png(bytes: &[u8]) -> Option<ImageContent> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    let (width, height) = (info.width as usize, info.height as usize);
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf
            .chunks_exact(3)
            .flat_map(|pixel| [pixel[0], pixel[1], pixel[2], 255])
            .collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&v| [v, v, v, 255]).collect(),
        png::ColorType::GrayscaleAlpha => buf
            .chunks_exact(2)
            .flat_map(|pixel| [pixel[0], pixel[0], pixel[0], pixel[1]])
            .collect(),
        png::ColorType::Indexed => return None,
    };
    let (out_width, out_height, small) = downscale_rgba(width, height, &rgba, MAX_IMAGE_DIM)?;
    let png = encode_png(out_width as u32, out_height as u32, &small).ok()?;
    Some(ImageContent::png(&png))
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

#[cfg(test)]
mod tests {
    use super::*;

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

        // A small png passes through untouched.
        let small = encode_png(4, 4, &[9u8; 4 * 4 * 4]).unwrap();
        let content = from_file_bytes(&small).unwrap();
        assert_eq!(content, ImageContent::png(&small));

        // An oversized png is decoded, downscaled and re-encoded.
        let pixels = vec![7u8; 3000 * 1000 * 4];
        let big = encode_png(3000, 1000, &pixels).unwrap();
        let content = from_file_bytes(&big).unwrap();
        let (width, height, downscaled) = downscale_rgba(3000, 1000, &pixels, 2048).unwrap();
        let expected = encode_png(width as u32, height as u32, &downscaled).unwrap();
        assert_eq!(content, ImageContent::png(&expected));
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
