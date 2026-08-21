//! Image utilities — MIME type detection, base64 encoding/decoding,
//! and cross-format image standardization.
//!
//! The standardization step ensures that ANY input format (TIFF, HEIC, BMP, …)
//! is converted to a well-known format (PNG by default) that all multimodal
//! backends accept before base64 encoding.

use std::io::Cursor;

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
    // BMP: 42 4D
    if bytes[0] == 0x42 && bytes[1] == 0x4D {
        return "image/bmp";
    }
    // TIFF: 49 49 2A 00 (little-endian) or 4D 4D 00 2A (big-endian)
    if bytes.len() >= 4
        && ((bytes[0] == 0x49 && bytes[1] == 0x49 && bytes[2] == 0x2A && bytes[3] == 0x00)
            || (bytes[0] == 0x4D && bytes[1] == 0x4D && bytes[2] == 0x00 && bytes[3] == 0x2A))
    {
        return "image/tiff";
    }

    "image/png"
}

/// Detect MIME type from a base64 data URI prefix.
/// E.g. `"data:image/jpeg;base64,..."` → `"image/jpeg"`.
///
/// Returns `None` if the string does not start with `data:` or has an
/// unrecognised prefix.
pub fn detect_mime_from_data_uri(data: &str) -> Option<&str> {
    let rest = data.strip_prefix("data:")?;
    let mime_end = rest.find(';')?;
    let mime = &rest[..mime_end];
    if mime.starts_with("image/") {
        Some(mime)
    } else {
        None
    }
}

/// Detect MIME type: try data URI first, then magic bytes.
pub fn detect_mime(data: &str) -> String {
    if let Some(mime) = detect_mime_from_data_uri(data) {
        return mime.to_string();
    }
    // Try decoding first ~16 bytes of base64 to check magic headers
    let first_chunk = if data.len() > 16 { &data[..16] } else { data };
    if let Ok(decoded) = simple_base64_decode(first_chunk) {
        return detect_mime_from_bytes(&decoded).to_string();
    }
    "image/png".to_string()
}

/// Standardize raw image bytes: decode any format, re-encode as PNG.
///
/// This ensures exotic formats (TIFF, BMP, HEIC, etc.) are converted to
/// a common format before sending to the multimodal backend.  PNG is chosen
/// because it's lossless and universally supported.
///
/// Returns `(standardized_bytes, "image/png")`.
/// On failure, returns the original bytes with a best-guess MIME type.
pub fn standardize_image(raw_bytes: &[u8]) -> (Vec<u8>, String) {
    match try_standardize(raw_bytes, image::ImageFormat::Png) {
        Ok(data) => (data, "image/png".to_string()),
        Err(_) => {
            // Fallback: keep original bytes, detect MIME from magic
            let mime = detect_mime_from_bytes(raw_bytes).to_string();
            (raw_bytes.to_vec(), mime)
        }
    }
}

/// Standardize to a specific format.
fn try_standardize(
    raw_bytes: &[u8],
    target_format: image::ImageFormat,
) -> Result<Vec<u8>, image::ImageError> {
    let img = image::ImageReader::new(Cursor::new(raw_bytes))
        .with_guessed_format()?
        .decode()?;

    let mut output = Cursor::new(Vec::new());
    img.write_to(&mut output, target_format)?;
    Ok(output.into_inner())
}

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

/// Minimal base64 decode (for magic-byte detection only).
/// Decodes at most a few bytes; not suitable for large payloads.
fn simple_base64_decode(input: &str) -> Result<Vec<u8>, ()> {
    let mut output = Vec::new();
    let mut buffer = 0u32;
    let mut bits_collected = 0u32;

    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => continue,
        } as u32;

        buffer = (buffer << 6) | value;
        bits_collected += 6;

        if bits_collected >= 8 {
            bits_collected -= 8;
            output.push((buffer >> bits_collected) as u8);
            buffer &= (1 << bits_collected) - 1;
        }
    }
    Ok(output)
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // A minimal valid PNG (1×1 pixel, red, uncompressed)
    fn make_test_png() -> Vec<u8> {
        // 1×1 red pixel PNG
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
        let png = make_test_png();
        assert_eq!(detect_mime_from_bytes(&png), "image/png");
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
    fn detect_bmp_magic() {
        let bmp = vec![0x42, 0x4D, 0x00, 0x00];
        assert_eq!(detect_mime_from_bytes(&bmp), "image/bmp");
    }

    #[test]
    fn detect_tiff_magic_le() {
        let tiff = vec![0x49, 0x49, 0x2A, 0x00];
        assert_eq!(detect_mime_from_bytes(&tiff), "image/tiff");
    }

    #[test]
    fn detect_tiff_magic_be() {
        let tiff = vec![0x4D, 0x4D, 0x00, 0x2A];
        assert_eq!(detect_mime_from_bytes(&tiff), "image/tiff");
    }

    #[test]
    fn detect_mime_from_data_uri_prefix() {
        assert_eq!(
            detect_mime_from_data_uri("data:image/jpeg;base64,/9j/4AAQ"),
            Some("image/jpeg")
        );
        assert_eq!(
            detect_mime_from_data_uri("data:image/png;base64,iVBORw"),
            Some("image/png")
        );
        assert_eq!(detect_mime_from_data_uri("not a data uri"), None);
    }

    #[test]
    fn base64_encode_decode_roundtrip() {
        let original = b"Hello, world! This is a test.";
        let encoded = encode_base64(original);
        let decoded = simple_base64_decode(&encoded).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn base64_encode_png() {
        let png = make_test_png();
        let b64 = encode_base64(&png);
        // Should be valid base64 (no special chars)
        assert!(
            b64.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=')
        );
        // Decode should match original
        let decoded = simple_base64_decode(&b64).unwrap();
        assert_eq!(decoded, png);
    }

    #[test]
    fn base64_padding() {
        // 1 byte → 4 chars with "=="
        assert_eq!(encode_base64(b"f"), "Zg==");
        // 2 bytes → 4 chars with "="
        assert_eq!(encode_base64(b"fo"), "Zm8=");
        // 3 bytes → 4 chars
        assert_eq!(encode_base64(b"foo"), "Zm9v");
    }

    #[test]
    fn standardize_png_identity() {
        let png = make_test_png();
        let (std_bytes, mime) = standardize_image(&png);
        assert_eq!(mime, "image/png");
        // Re-standardized PNG should still be valid
        let mime2 = detect_mime_from_bytes(&std_bytes);
        assert_eq!(mime2, "image/png");
    }

    #[test]
    fn standardize_unknown_bytes_fallback() {
        let garbage = vec![0x00, 0x01, 0x02, 0x03];
        let (bytes, mime) = standardize_image(&garbage);
        // Should fall back to original bytes
        assert_eq!(bytes, garbage);
        assert_eq!(mime, "image/png"); // default fallback
    }

    #[test]
    fn full_pipeline_base64_roundtrip() {
        // Simulate: receive raw image → standardize → base64 → detect
        let png = make_test_png();
        let (std_bytes, mime) = standardize_image(&png);
        assert_eq!(mime, "image/png");

        let b64 = encode_base64(&std_bytes);
        let detected = detect_mime(&b64);
        assert_eq!(detected, "image/png");
    }
}
