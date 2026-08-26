//! Image utilities — MIME detection, base64 codec, and normalization
//! (downscale + re-compress) for model-bound image payloads.

use std::io::Cursor;

/// Longest edge allowed before downscaling (mirrors opencode's policy).
pub const MAX_DIMENSION: u32 = 2000;
/// Base64 budget for a normalized payload.
pub const MAX_BASE64_BYTES: usize = 5 * 1024 * 1024;
/// Raw-byte equivalent of the base64 budget.
const MAX_RAW_BYTES: usize = MAX_BASE64_BYTES / 4 * 3;

/// Detect MIME type from raw image bytes by checking magic headers.
/// Falls back to `"image/png"` when detection fails.
pub fn detect_mime_from_bytes(bytes: &[u8]) -> &'static str {
    if bytes.len() < 4 {
        return "image/png";
    }
    // JPEG: FF D8 FF
    if bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF {
        return "image/jpeg";
    }
    // PNG: 89 50 4E 47
    if bytes[0] == 0x89 && bytes[1] == 0x50 && bytes[2] == 0x4E && bytes[3] == 0x47 {
        return "image/png";
    }
    // GIF: 47 49 46
    if bytes[0] == 0x47 && bytes[1] == 0x49 && bytes[2] == 0x46 {
        return "image/gif";
    }
    // WebP: 52 49 46 46 … 57 45 42 50 at offset 8
    if bytes.len() >= 12
        && bytes[0] == 0x52
        && bytes[1] == 0x49
        && bytes[2] == 0x46
        && bytes[3] == 0x46
        && bytes[8] == 0x57
        && bytes[9] == 0x45
        && bytes[10] == 0x42
        && bytes[11] == 0x50
    {
        return "image/webp";
    }

    "image/png"
}

// ── Normalization (downscale + re-compress) ──────────────────────────

/// A normalized image ready to be attached to a tool result.
pub struct NormalizedImage {
    pub bytes: Vec<u8>,
    /// Always `image/png` or `image/jpeg` after normalization.
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
}

/// Normalize raw image bytes for the model:
/// - decode any supported format (PNG/JPEG/GIF/WebP/BMP/TIFF…)
/// - downscale to fit [`MAX_DIMENSION`] on the longest edge
/// - re-encode until the base64 payload fits [`MAX_BASE64_BYTES`]
///
/// Already-small PNG/JPEG inputs pass through untouched (no lossy re-encode).
/// Animated GIFs collapse to their first frame. Errors on undecodable input.
pub fn normalize_image(raw: &[u8]) -> Result<NormalizedImage, String> {
    let img = image::ImageReader::new(Cursor::new(raw))
        .with_guessed_format()
        .map_err(|e| format!("cannot sniff image format: {e}"))?
        .decode()
        .map_err(|e| format!("not a decodable image: {e}"))?;

    let (width, height) = (img.width(), img.height());
    let oversized = width.max(height) > MAX_DIMENSION;
    let over_budget = raw.len() > MAX_RAW_BYTES;

    if !oversized && !over_budget {
        let mime = detect_mime_from_bytes(raw);
        if mime == "image/png" || mime == "image/jpeg" {
            return Ok(NormalizedImage {
                bytes: raw.to_vec(),
                mime,
                width,
                height,
            });
        }
    }

    let img = if oversized {
        img.resize(
            MAX_DIMENSION,
            MAX_DIMENSION,
            image::imageops::FilterType::Triangle,
        )
    } else {
        img
    };

    let (width, height) = (img.width(), img.height());

    // Try PNG first (lossless), then JPEG with descending quality.
    if let Some(png) = encode_png(&img) {
        return Ok(NormalizedImage {
            bytes: png,
            mime: "image/png",
            width,
            height,
        });
    }
    for quality in [85u8, 75, 60] {
        if let Some(jpeg) = encode_jpeg(&img, quality) {
            return Ok(NormalizedImage {
                bytes: jpeg,
                mime: "image/jpeg",
                width,
                height,
            });
        }
    }
    Err(format!(
        "image still exceeds the {} MB budget after compression",
        MAX_BASE64_BYTES / 1024 / 1024
    ))
}

fn within_budget(bytes: &[u8]) -> bool {
    // base64 expands 3 raw bytes → 4 chars; compare in the raw domain.
    bytes.len().div_ceil(3) * 4 <= MAX_BASE64_BYTES
}

fn encode_png(img: &image::DynamicImage) -> Option<Vec<u8>> {
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).ok()?;
    let bytes = out.into_inner();
    within_budget(&bytes).then_some(bytes)
}

fn encode_jpeg(img: &image::DynamicImage, quality: u8) -> Option<Vec<u8>> {
    let rgb = img.to_rgb8();
    let (width, height) = rgb.dimensions();
    let mut out = Cursor::new(Vec::new());
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
    encoder
        .encode(rgb.as_raw(), width, height, image::ExtendedColorType::Rgb8)
        .ok()?;
    let bytes = out.into_inner();
    within_budget(&bytes).then_some(bytes)
}

// ── Base64 ────────────────────────────────────────────────────────────

/// Encode arbitrary bytes to a base64 string.
pub fn encode_base64(bytes: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;

        output.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
        output.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            output.push(CHARS[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            output.push('=');
        }
        if chunk.len() > 2 {
            output.push(CHARS[(triple & 0x3F) as usize] as char);
        } else {
            output.push('=');
        }
    }
    output
}

/// Minimal RFC 4648 base64 decode (padding-tolerant, ignores whitespace).
pub fn decode_base64(input: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for ch in input.bytes() {
        if ch == b'=' {
            break;
        }
        let v = match ch {
            b'A'..=b'Z' => (ch - b'A') as u32,
            b'a'..=b'z' => (ch - b'a') as u32 + 26,
            b'0'..=b'9' => (ch - b'0') as u32 + 52,
            b'+' => 62,
            b'/' => 63,
            b'\r' | b'\n' | b' ' | b'\t' => continue,
            _ => return Err(format!("invalid base64 char: {ch}")),
        };
        buffer = (buffer << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_png() -> Vec<u8> {
        vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // signature
            0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, // IHDR
            0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, // 1×1
            0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53, 0xDE, // IHDR CRC
            0x00, 0x00, 0x00, 0x0E, 0x49, 0x44, 0x41, 0x54, // IDAT
            0x78, 0x9C, 0x62, 0x60, 0x60, 0x60, 0x00, 0x00, 0x00, 0x04, 0x00, 0x01, 0x27, 0x34,
            0x03, 0x7A, // IDAT data + CRC
            0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, // IEND
            0xAE, 0x42, 0x60, 0x82, // IEND CRC
        ]
    }

    #[test]
    fn detect_png_magic() {
        assert_eq!(detect_mime_from_bytes(&make_test_png()), "image/png");
    }

    #[test]
    fn detect_jpeg_magic() {
        let jpeg_header = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
        assert_eq!(detect_mime_from_bytes(&jpeg_header), "image/jpeg");
    }

    #[test]
    fn detect_gif_magic() {
        let gif_header = vec![0x47, 0x49, 0x46, 0x38, 0x39, 0x61];
        assert_eq!(detect_mime_from_bytes(&gif_header), "image/gif");
    }

    #[test]
    fn detect_webp_magic() {
        let webp = b"RIFF\x00\x00\x00\x00WEBP";
        assert_eq!(detect_mime_from_bytes(webp), "image/webp");
    }

    #[test]
    fn detect_short_input_falls_back_to_png() {
        assert_eq!(detect_mime_from_bytes(&[0x01, 0x02]), "image/png");
        assert_eq!(detect_mime_from_bytes(&[]), "image/png");
    }

    #[test]
    fn base64_padding() {
        assert_eq!(encode_base64(b"f"), "Zg==");
        assert_eq!(encode_base64(b"fo"), "Zm8=");
        assert_eq!(encode_base64(b"foo"), "Zm9v");
    }

    #[test]
    fn base64_encode_png_prefix() {
        let b64 = encode_base64(&make_test_png());
        assert!(b64.starts_with("iVBORw0KGgo"));
    }

    #[test]
    fn base64_roundtrip() {
        for sample in [&b""[..], b"f", b"fo", b"foo", b"foobar", "界面".as_bytes()] {
            assert_eq!(
                decode_base64(&encode_base64(sample)).unwrap(),
                sample,
                "sample: {sample:?}"
            );
        }
    }

    /// A real, decodable 1×1 PNG generated by the image crate.
    fn real_png() -> Vec<u8> {
        let img = image::RgbImage::new(1, 1);
        let mut out = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    #[test]
    fn small_png_passes_through_untouched() {
        let png = real_png();
        let normalized = normalize_image(&png).expect("normalize");
        assert_eq!(normalized.mime, "image/png");
        assert_eq!(
            normalized.bytes, png,
            "already-small input must not re-encode"
        );
        assert_eq!((normalized.width, normalized.height), (1, 1));
    }

    #[test]
    fn oversized_image_is_downscaled() {
        // 3000×10 gradient PNG → longest edge must clamp to MAX_DIMENSION.
        let buffer =
            image::RgbImage::from_fn(3000, 10, |x, _y| image::Rgb([(x % 256) as u8, 64, 128]));
        let mut png = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(buffer)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let raw = png.into_inner();

        let normalized = normalize_image(&raw).expect("normalize");
        assert_eq!(normalized.mime, "image/png");
        let decoded = image::load_from_memory(&normalized.bytes).unwrap();
        assert!(decoded.width() <= MAX_DIMENSION);
        assert!(
            decoded.height() < 10,
            "box-fit resize must shrink the short edge too, got {}",
            decoded.height()
        );
    }

    #[test]
    fn garbage_input_is_rejected() {
        assert!(normalize_image(b"not an image at all").is_err());
    }
}
