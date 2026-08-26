//! `read_image` tool — load an image into the model's own visual context.
//!
//! Unlike the former `image_query` tool (which forwarded images to an
//! external vision model), `read_image` attaches the image directly to the
//! tool result message. The gate lowers attached images into provider-native
//! media parts (`image_url` / `input_image`) on the next request, so a
//! vision-capable main model sees the pixels itself.
//!
//! Sources:
//! - `image_index`: an image uploaded in the current conversation
//!   (registered by engine_input when the user attaches one).
//! - `path`: an image file inside the workspace (admission authorizes the
//!   path resource before the handler runs).
//!
//! The tool is only exposed to endpoints that declare vision input support
//! ([`image_tool_enabled`], currently opencode-go only).

pub mod image_utils;

use crate::{ToolCallCtx, ToolHandler, ToolResult, ToolRisk};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

/// Raw byte cap before decoding (~20 MB).
const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

// ── Global image registry ─────────────────────────────────────────────
///
/// Stores uploaded images keyed by session seed so read_image can look them
/// up by index without the LLM needing the raw base64 data. Images are
/// **peeked** (cloned) on lookup and never consumed — repeated reads across
/// turns must keep working while the upload stays in context.
static IMAGE_REGISTRY: std::sync::LazyLock<Mutex<HashMap<String, Vec<ImageEntry>>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone)]
struct ImageEntry {
    mime_type: String,
    data: String,
}

/// Register an uploaded image for a session. Called from engine_input.
pub fn store_image(seed: &str, mime_type: &str, data: &str) {
    if let Ok(mut reg) = IMAGE_REGISTRY.lock() {
        reg.entry(seed.to_string()).or_default().push(ImageEntry {
            mime_type: mime_type.to_string(),
            data: data.to_string(),
        });
    }
}

/// Drop all registered images for a session.
///
/// Called before rebuilding the registry from persisted message history
/// (session restore) so repeated restores never shift the indices.
pub fn reset_images(seed: &str) {
    if let Ok(mut reg) = IMAGE_REGISTRY.lock() {
        reg.remove(seed);
    }
}

/// Peek at an image by index — returns a clone without removing.
pub fn peek_image(seed: &str, index: usize) -> Option<(String, String)> {
    let reg = IMAGE_REGISTRY.lock().ok()?;
    let entries = reg.get(seed)?;
    entries
        .get(index)
        .map(|e| (e.mime_type.clone(), e.data.clone()))
}

// ── Capability gate ───────────────────────────────────────────────────

/// Whether the currently configured provider endpoint accepts image input.
///
/// Single source of truth is the provider registry
/// ([`qaqh_config::registry::image_tool_enabled`], probed via
/// [`crate::runtime::image_tool_enabled`]). Unknown/unloadable configs fail
/// closed (tool hidden).
use crate::runtime::image_tool_enabled;

// ── Main handler ──────────────────────────────────────────────────────

/// Handle the `read_image` tool call.
///
/// Exactly one of `image_index` / `path` must be provided. On success the
/// returned [`ToolResult`] carries the image in `images`; the message layer
/// appends it to the tool message and the gate lowers it to media parts.
///
/// Every payload passes through [`image_utils::normalize_image`]: oversized
/// images are downscaled / re-compressed before entering the conversation.
pub(super) fn handle_read_image(ctx: ToolCallCtx) -> ToolResult {
    if !image_tool_enabled() {
        return ToolResult::error("read_image: the active provider does not support image input");
    }

    let index = ctx.get_u64("image_index");
    let path_arg = ctx.get_str("path").unwrap_or_default().to_string();

    // ── Resolve raw bytes ──
    let (raw_bytes, display) = if let Some(idx) = index {
        let idx = idx as usize;
        let seed = match crate::runtime::context() {
            Some(c) => c.active_session,
            None => {
                return ToolResult::error(
                    "read_image: no active session — image_index requires a running session context",
                );
            }
        };
        match peek_image(&seed, idx) {
            Some((_mime, data)) => {
                // 上传侧无大小校验（Electron main 原样透传），这里兜底：
                // 拒绝异常巨大的 base64，避免无谓的解码开销。
                if data.len() > image_utils::MAX_BASE64_BYTES * 4 {
                    return ToolResult::error(format!(
                        "read_image: upload #{idx} is too large ({}, limit ~{} bytes)",
                        data.len(),
                        image_utils::MAX_BASE64_BYTES * 4
                    ));
                }
                let raw = match image_utils::decode_base64(&data) {
                    Ok(raw) => raw,
                    Err(e) => {
                        return ToolResult::error(format!(
                            "read_image: upload #{idx} has invalid base64: {e}"
                        ));
                    }
                };
                (raw, format!("upload #{idx}"))
            }
            None => {
                return ToolResult::error(format!(
                    "read_image: image_index {idx} not found in session '{seed}'. \
                     The upload may have left the context. Ask the user to re-attach it."
                ));
            }
        }
    } else if !path_arg.is_empty() {
        match read_image_file(&path_arg) {
            Ok(pair) => pair,
            Err(err) => return ToolResult::error(format!("read_image: {err}")),
        }
    } else {
        return ToolResult::error(
            "read_image: either image_index or path is required. \
             If you see [Image #N: ...] in the conversation, use image_index=N.",
        );
    };

    // ── Normalize (downscale + re-compress) ──
    let normalized = match image_utils::normalize_image(&raw_bytes) {
        Ok(n) => n,
        Err(e) => return ToolResult::error(format!("read_image: {display}: {e}")),
    };
    let mime_type = normalized.mime;
    let data = image_utils::encode_base64(&normalized.bytes);

    ToolResult::ok(format!(
        "Image read successfully: {display} ({mime_type}, {}×{}, {} bytes base64 after normalization). \
         The image is attached to this tool result and visible to you.",
        normalized.width,
        normalized.height,
        data.len()
    ))
    .with_image(mime_type.to_string(), data)
}

/// Read an image file from disk (workspace-relative or absolute).
fn read_image_file(path: &str) -> Result<(Vec<u8>, String), String> {
    let root = crate::runtime::active_workspace_root();
    let candidate = Path::new(path);
    let full = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };

    let meta =
        std::fs::metadata(&full).map_err(|e| format!("cannot stat '{}': {e}", full.display()))?;
    if !meta.is_file() {
        return Err(format!("'{}' is not a regular file", full.display()));
    }
    if meta.len() as usize > MAX_IMAGE_BYTES {
        return Err(format!(
            "'{}' is too large ({} bytes, max ~{MAX_IMAGE_BYTES})",
            full.display(),
            meta.len()
        ));
    }

    let bytes =
        std::fs::read(&full).map_err(|e| format!("cannot read '{}': {e}", full.display()))?;
    Ok((bytes, full.display().to_string()))
}

// ── Registration ──────────────────────────────────────────────────────

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register_with_placement(ToolHandler {
        key: "read_image".to_string(),
        description: "Load an image into your own visual context so you can see it directly. \
             Use image_index to view an image the user uploaded ([Image #N: ...] references), \
             or path to view an image file in the workspace (png/jpeg/gif/webp/bmp/tiff). \
             Oversized images are downscaled and re-compressed automatically. \
             Do NOT pass base64 data — reference images by index or path only.",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "image_index": {
                    "type": "integer",
                    "description": "Index of an image uploaded in this conversation (0-based). Use this when you see [Image #N: ...] references."
                },
                "path": {
                    "type": "string",
                    "description": "Path to an image file (workspace-relative or absolute)."
                }
            },
            "additionalProperties": false,
            "anyOf": [
                { "required": ["image_index"], "description": "View an image already uploaded in this conversation" },
                { "required": ["path"], "description": "View an image file from the workspace" }
            ]
        }),
        handler: handle_read_image,
        risk: ToolRisk::ReadOnly,
        category: crate::permission::ToolCategory::Read,
        default_timeout: std::time::Duration::from_secs(30),
    }, crate::ToolPlacement::Workspace);
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal valid PNG (1×1 pixel, red)
    fn test_png_bytes() -> Vec<u8> {
        vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
            0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0E, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x62, 0x60, 0x60, 0x60, 0x00, 0x00, 0x00, 0x04, 0x00, 0x01, 0x27, 0x34, 0x03,
            0x7A, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ]
    }

    #[test]
    fn registry_peek_is_non_destructive() {
        let seed = "read_image_registry_test";
        store_image(seed, "image/png", "Zm9v");
        assert_eq!(
            peek_image(seed, 0),
            Some(("image/png".into(), "Zm9v".into()))
        );
        // Peek again — must still be there (no consume semantics).
        assert_eq!(
            peek_image(seed, 0),
            Some(("image/png".into(), "Zm9v".into()))
        );
        assert_eq!(peek_image(seed, 1), None);
        assert_eq!(peek_image("other-seed", 0), None);
    }

    #[test]
    fn reset_then_replay_keeps_indices_stable() {
        // 模拟 session restore：先 reset 再按历史顺序重放注册，
        // 重复 restore 不产生重复条目、不移动索引。
        let seed = "read_image_reset_test";
        store_image(seed, "image/png", "AAA");
        store_image(seed, "image/jpeg", "BBB");
        reset_images(seed);
        reset_images(seed); // 幂等
        store_image(seed, "image/png", "AAA");
        store_image(seed, "image/jpeg", "BBB");
        assert_eq!(
            peek_image(seed, 0),
            Some(("image/png".into(), "AAA".into()))
        );
        assert_eq!(
            peek_image(seed, 1),
            Some(("image/jpeg".into(), "BBB".into()))
        );
        assert_eq!(peek_image(seed, 2), None);
    }

    #[test]
    fn missing_args_fail_without_session() {
        // No args at all → argument error (before any capability check side
        // effects beyond the enabled probe).
        let ctx = crate::ToolCallCtx {
            id: "read-image-test".into(),
            name: "read_image".into(),
            action: String::new(),
            args: serde_json::json!({}),
            tx_progress: None,
            timeout_secs: Some(30),
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            skill_effects: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        };
        let result = handle_read_image(ctx);
        assert!(!result.is_success());
    }

    #[test]
    fn png_magic_detected_and_encoded() {
        let bytes = test_png_bytes();
        assert_eq!(image_utils::detect_mime_from_bytes(&bytes), "image/png");
        let b64 = image_utils::encode_base64(&bytes);
        assert!(b64.starts_with("iVBORw0KGgo"));
    }
}
