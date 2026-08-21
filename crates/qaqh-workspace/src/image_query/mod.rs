//! Image query tool — multi-modal image understanding via pluggable backends.
//!
//! ## Architecture
//!
//! ```text
//! handle_image_query()
//!   ├─ validate_input()        ← 公共
//!   ├─ resolve_config()         ← 公共
//!   ├─ select_backend(cfg)      ← 调度 → Box<dyn MultimodalBackend>
//!   ├─ backend.build_request()  ← 适配点 #1
//!   ├─ backend.auth_headers()   ← 适配点 #2
//!   ├─ send_request()           ← 公共
//!   ├─ backend.extract_content()← 适配点 #3
//!   └─ format_result()          ← 公共
//! ```
//!
//! ## Supported backends
//!
//! | provider_type   | Backend               | Auth        | Endpoint            |
//! |-----------------|-----------------------|-------------|---------------------|
//! | `"mimo"`        | MiMoBackend           | api-key     | /chat/completions|
//! | `"ollama"`      | OllamaBackend         | none        | /api/chat           |
//! | `"lmstudio"`    | LmStudioBackend       | Bearer(opt) | /api/v1/chat        |
//! | `"openai_compat"`| OpenAiCompatBackend   | Bearer      | /chat/completions|
//! | `""` (default)  | MiMoBackend           | api-key     | /chat/completions|

pub mod backend;
pub mod image_utils;
pub mod lmstudio;
pub mod mimo;
pub mod ollama;
pub mod openai_compat;

use crate::{ToolCallCtx, ToolHandler, ToolResult, ToolRisk};
use backend::MultimodalBackend;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

// ── Global image registry ─────────────────────────────────────────────
///
/// Stores uploaded images keyed by session seed so image_query can
/// look them up by index without the LLM needing the raw base64 data.
///
/// Images are **peeked** (cloned) on lookup and only **consumed** (removed)
/// after a successful API call, so failed requests can be retried.

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

/// Peek at an image by index — returns a clone without removing.
/// Use `consume_image` to remove after a successful API call.
pub fn peek_image(seed: &str, index: usize) -> Option<(String, String)> {
    let reg = IMAGE_REGISTRY.lock().ok()?;
    let entries = reg.get(seed)?;
    entries
        .get(index)
        .map(|e| (e.mime_type.clone(), e.data.clone()))
}

/// Remove a specific image from the registry after successful processing.
pub fn consume_image(seed: &str, index: usize) {
    if let Ok(mut reg) = IMAGE_REGISTRY.lock()
        && let Some(entries) = reg.get_mut(seed)
            && index < entries.len() {
                entries.remove(index);
                if entries.is_empty() {
                    reg.remove(seed);
                }
            }
}

// ── Backend selector ──────────────────────────────────────────────────

fn select_backend(provider_type: &str) -> Box<dyn MultimodalBackend> {
    match provider_type {
        "ollama" => Box::new(ollama::OllamaBackend),
        "lmstudio" => Box::new(lmstudio::LmStudioBackend),
        "openai_compat" => Box::new(openai_compat::OpenAiCompatBackend),
        _ => Box::new(mimo::MiMoBackend), // "mimo" or empty → default
    }
}

// ── Shared HTTP agent ─────────────────────────────────────────────────

fn http_agent(timeout_secs: u64) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(timeout_secs.max(30))))
        .build()
        .into()
}

// ── Main handler ──────────────────────────────────────────────────────

/// Handle the `image_query` tool call.
///
/// Two modes:
/// - `image_index` (recommended): Looks up the image from the session registry.
///   This is how the LLM should call it after seeing an `[Image #N: ...]` reference.
/// - `base64_image` (fallback): Direct base64 data, for manual / programmatic use.
///
/// Always requires `prompt`.
pub(super) fn handle_image_query(ctx: ToolCallCtx) -> ToolResult {
    // ── 1. Validate input ──
    let prompt = ctx.get_str("prompt").unwrap_or_default().to_string();

    if prompt.is_empty() {
        return ToolResult::error("image: prompt is required");
    }

    // Resolve image data: prefer image_index (lookup), fall back to base64_image
    let (image_data, mime_type, image_seed, image_idx) =
        if let Some(idx) = ctx.get_u64("image_index") {
            let idx = idx as usize;
            let seed = match crate::runtime::context() {
                Some(c) => c.active_session,
                None => {
                    return ToolResult::error(
                        "image: no active session — image_index requires a running session context",
                    );
                }
            };
            match peek_image(&seed, idx) {
                Some((mime, data)) => (data, mime, Some(seed), Some(idx)),
                None => {
                    return ToolResult::error(format!(
                        "image: image_index {idx} not found in session '{seed}'. \
                         Images may have been uploaded in a different turn. \
                         Try re-uploading the image."
                    ));
                }
            }
        } else {
            let data = ctx.get_str("base64_image").unwrap_or_default().to_string();
            if data.is_empty() {
                return ToolResult::error(
                    "image: either image_index or base64_image is required. \
                     Tip: if you see [Image #N: ...] in the conversation, use image_index=N.",
                );
            }
            let mime = image_utils::detect_mime(&data);
            (data, mime, None, None)
        };

    // ── 2. Load config ──
    let cfg = match qaqh_config::Config::load() {
        Ok(c) => c,
        Err(e) => return ToolResult::error(format!("image: failed to load config: {e}")),
    };

    if !cfg.multimodal.enabled {
        return ToolResult::error(
            "image: multimodal is not enabled. \
             Please configure a multimodal provider in Settings > Multimodal.",
        );
    }

    // ── 3. Select backend ──
    let backend = select_backend(&cfg.multimodal.provider_type);

    // ── 4. Resolve credentials ──
    let api_key = if cfg.multimodal.api_key.is_empty() {
        cfg.api_key.clone()
    } else {
        cfg.multimodal.api_key.clone()
    };

    let base_url = if cfg.multimodal.base_url.is_empty() {
        // Use default URL based on provider type
        match cfg.multimodal.provider_type.as_str() {
            "mimo" | "" => "https://api.xiaomimimo.com/v1".to_string(),
            "ollama" => "http://localhost:11434".to_string(),
            "lmstudio" => "http://localhost:1234".to_string(),
            _ => cfg.base_url.clone(),
        }
    } else {
        cfg.multimodal.base_url.clone()
    };

    let model = cfg.multimodal.model.clone();
    let max_tokens = cfg.multimodal.max_tokens;

    // ── 5. Validate size ──
    let max_b64 = backend.max_image_bytes().saturating_mul(4) / 3;
    if image_data.len() > max_b64 {
        return ToolResult::error(format!(
            "image: base64 data too large ({} bytes, max ~{} bytes for this backend)",
            image_data.len(),
            max_b64
        ));
    }

    // ── 6. Build request ──
    let body = backend.build_request(&model, &mime_type, &image_data, &prompt, max_tokens);

    // ── 7. Send request ──
    let timeout_secs = ctx.timeout_secs.unwrap_or(backend.default_timeout_secs());
    let url = format!(
        "{}{}",
        base_url.trim_end_matches('/'),
        backend.endpoint_path()
    );

    let mut req = http_agent(timeout_secs)
        .post(&url)
        .header("Content-Type", "application/json");
    for (name, value) in backend.auth_headers(&api_key) {
        req = req.header(&name, &value);
    }

    let resp = match req.send_json(&body) {
        Ok(r) => r,
        Err(e) => {
            return ToolResult::error(format!("image: API request failed: {e}"));
        }
    };

    // ── 8. Check HTTP status & read body ──
    let status = resp.status();
    let body_bytes = match resp.into_body().read_to_vec() {
        Ok(b) => b,
        Err(e) => {
            return ToolResult::error(format!("image: failed to read response body: {e}"));
        }
    };

    if !(200..300).contains(&status.as_u16()) {
        let body_str = String::from_utf8_lossy(&body_bytes);
        // Try to extract a readable error message from the JSON body
        let err_msg = serde_json::from_slice::<serde_json::Value>(&body_bytes)
            .ok()
            .and_then(|v| {
                v.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| body_str[..body_str.len().min(500)].to_string());
        return ToolResult::error(format!(
            "image: API returned HTTP {}: {err_msg}",
            status.as_u16(),
        ));
    }

    // ── 9. Parse response ──
    let json: serde_json::Value = match serde_json::from_slice(&body_bytes) {
        Ok(j) => j,
        Err(e) => {
            let body_str = String::from_utf8_lossy(&body_bytes);
            return ToolResult::error(format!(
                "image: failed to parse JSON response: {e}. Body: {}",
                &body_str[..body_str.len().min(500)]
            ));
        }
    };

    // ── 10. Extract content ──
    let content = match backend.extract_content(&json) {
        Some(c) => c,
        None => {
            let err_msg = json
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("[No content in response]");
            return ToolResult::error(format!("image: {err_msg}"));
        }
    };

    // ── 11. Consume image on success ──
    if let (Some(seed), Some(idx)) = (image_seed.as_ref(), image_idx) {
        consume_image(seed, idx);
    }

    // ── 12. Format result ──
    let mut result = content.to_string();
    if let Some((prompt_tokens, completion_tokens)) = backend.extract_usage(&json) {
        result.push_str(&format!(
            "\n\n[Token usage: {} prompt + {} completion = {} total]",
            prompt_tokens,
            completion_tokens,
            prompt_tokens + completion_tokens
        ));
    }

    ToolResult::ok(&result)
}

// ── Registration ──────────────────────────────────────────────────────

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register_with_placement(ToolHandler {
        key: "image".to_string(),
        description: "Analyze an image using a multimodal vision model. Use image_index (preferred) to reference an uploaded image, or base64_image for direct data. Always provide a prompt describing what to analyze. Supports MiMo, Ollama, LM Studio, and OpenAI-compatible endpoints.",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "image_index": {
                    "type": "integer",
                    "description": "Index of an uploaded image from the current conversation (0-based). Use this when you see [Image #N: ...] references. Mutually exclusive with base64_image."
                },
                "base64_image": {
                    "type": "string",
                    "description": "Base64-encoded image data (without the data:image/...;base64, prefix). Use only when image_index is not applicable."
                },
                "prompt": {
                    "type": "string",
                    "description": "Text prompt describing what to analyze or ask about the image"
                }
            },
            "required": ["prompt"],
            "additionalProperties": false,
            "anyOf": [
                { "required": ["image_index"], "description": "Reference an image already uploaded in this conversation" },
                { "required": ["base64_image"], "description": "Pass raw image data inline" }
            ]
        }),
        handler: handle_image_query,
        risk: ToolRisk::ReadOnly,
        category: crate::permission::ToolCategory::Read,
        default_timeout: Duration::from_secs(300),
    },
    crate::ToolPlacement::Workspace,
);
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

    fn test_png_base64() -> String {
        image_utils::encode_base64(&test_png_bytes())
    }

    // ── Backend trait tests ───────────────────────────────────────────

    #[test]
    fn mimo_builds_openai_format() {
        let backend = mimo::MiMoBackend::default();
        let body = backend.build_request("mimo-v2.5", "image/png", "Zm9v", "describe", 1024);

        assert_eq!(body["model"], "mimo-v2.5");
        assert_eq!(body["messages"][0]["content"][0]["type"], "image_url");
        assert_eq!(body["messages"][0]["content"][1]["type"], "text");
        let url = body["messages"][0]["content"][0]["image_url"]["url"]
            .as_str()
            .unwrap();
        assert!(url.starts_with("data:image/png;base64,Zm9v"));
        assert_eq!(body["max_completion_tokens"], 1024);
    }

    #[test]
    fn mimo_uses_api_key_header() {
        let backend = mimo::MiMoBackend::default();
        let headers = backend.auth_headers("test-key");
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].0, "api-key");
        assert_eq!(headers[0].1, "test-key");
    }

    #[test]
    fn mimo_endpoint_path() {
        let backend = mimo::MiMoBackend::default();
        assert_eq!(backend.endpoint_path(), "/chat/completions");
    }

    #[test]
    fn openai_compat_uses_bearer() {
        let backend = openai_compat::OpenAiCompatBackend::default();
        let headers = backend.auth_headers("sk-123");
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].0, "Authorization");
        assert_eq!(headers[0].1, "Bearer sk-123");
    }

    #[test]
    fn openai_compat_no_auth_when_key_empty() {
        let backend = openai_compat::OpenAiCompatBackend::default();
        let headers = backend.auth_headers("");
        assert!(headers.is_empty());
    }

    #[test]
    fn ollama_builds_native_format() {
        let backend = ollama::OllamaBackend::default();
        let body = backend.build_request("llava:13b", "image/png", "Zm9v", "describe", 1024);

        assert_eq!(body["model"], "llava:13b");
        assert_eq!(body["stream"], false);
        assert_eq!(body["messages"][0]["images"][0], "Zm9v");
        // Ollama puts the prompt directly in content, not in a content array
        assert_eq!(body["messages"][0]["content"], "describe");
    }

    #[test]
    fn ollama_no_auth() {
        let backend = ollama::OllamaBackend::default();
        assert!(backend.auth_headers("anything").is_empty());
    }

    #[test]
    fn ollama_endpoint_path() {
        let backend = ollama::OllamaBackend::default();
        assert_eq!(backend.endpoint_path(), "/api/chat");
    }

    #[test]
    fn ollama_extract_response() {
        let backend = ollama::OllamaBackend::default();
        let json = serde_json::json!({
            "model": "llava:13b",
            "message": { "role": "assistant", "content": "This is a cat." },
            "prompt_eval_count": 120,
            "eval_count": 30
        });
        assert_eq!(backend.extract_content(&json), Some("This is a cat."));
        assert_eq!(backend.extract_usage(&json), Some((120, 30)));
    }

    // ── Response parsing tests ────────────────────────────────────────

    #[test]
    fn mimo_parses_openai_response() {
        let backend = mimo::MiMoBackend::default();
        let json = serde_json::json!({
            "choices": [{
                "message": { "content": "This image shows a sunset." }
            }],
            "usage": { "prompt_tokens": 50, "completion_tokens": 20 }
        });
        assert_eq!(
            backend.extract_content(&json),
            Some("This image shows a sunset.")
        );
        assert_eq!(backend.extract_usage(&json), Some((50, 20)));
    }

    #[test]
    fn mimo_handles_missing_content() {
        let backend = mimo::MiMoBackend::default();
        let json = serde_json::json!({ "error": "something went wrong" });
        assert_eq!(backend.extract_content(&json), None);
        assert_eq!(backend.extract_usage(&json), None);
    }

    #[test]
    fn ollama_handles_missing_content() {
        let backend = ollama::OllamaBackend::default();
        let json = serde_json::json!({ "error": "model not found" });
        assert_eq!(backend.extract_content(&json), None);
    }

    // ── Backend selector ──────────────────────────────────────────────

    #[test]
    fn select_backend_mimo() {
        let b = select_backend("mimo");
        assert_eq!(b.endpoint_path(), "/chat/completions");
        let headers = b.auth_headers("k");
        assert_eq!(headers[0].0, "api-key");
    }

    #[test]
    fn select_backend_ollama() {
        let b = select_backend("ollama");
        assert_eq!(b.endpoint_path(), "/api/chat");
        assert!(b.auth_headers("k").is_empty());
    }

    #[test]
    fn select_backend_lmstudio() {
        let b = select_backend("lmstudio");
        assert_eq!(b.endpoint_path(), "/api/v1/chat"); // LM Studio native endpoint
    }

    #[test]
    fn lmstudio_builds_native_format() {
        let backend = lmstudio::LmStudioBackend::default();
        let body = backend.build_request("qwen/qwen3-vl-4b", "image/png", "Zm9v", "describe", 1024);

        assert_eq!(body["model"], "qwen/qwen3-vl-4b");
        assert_eq!(body["input"][0]["type"], "text");
        assert_eq!(body["input"][0]["content"], "describe");
        assert_eq!(body["input"][1]["type"], "image");
        let data_url = body["input"][1]["data_url"].as_str().unwrap();
        assert!(data_url.starts_with("data:image/png;base64,Zm9v"));
        assert_eq!(body["context_length"], 4096);
        assert_eq!(body["temperature"], 0.0);
        // Reasoning must be explicitly disabled
        assert_eq!(body["reasoning"]["type"], "disabled");
    }

    #[test]
    fn lmstudio_parses_response() {
        let backend = lmstudio::LmStudioBackend::default();
        // Simulate response with reasoning BEFORE message
        let json = serde_json::json!({
            "output": [
                { "type": "reasoning", "content": "Let me think about this image..." },
                { "type": "message", "content": "A red square" }
            ],
            "stats": { "input_tokens": 17, "total_output_tokens": 30 }
        });
        // Must return the message content, NOT the reasoning
        assert_eq!(backend.extract_content(&json), Some("A red square"));
        assert_eq!(backend.extract_usage(&json), Some((17, 30)));
    }

    #[test]
    fn lmstudio_parses_message_only_response() {
        let backend = lmstudio::LmStudioBackend::default();
        // Response with only a message (no reasoning — when disabled)
        let json = serde_json::json!({
            "output": [
                { "type": "message", "content": "This is a cat" }
            ],
            "stats": { "input_tokens": 10, "total_output_tokens": 15 }
        });
        assert_eq!(backend.extract_content(&json), Some("This is a cat"));
    }

    #[test]
    fn lmstudio_no_auth_when_key_empty() {
        let backend = lmstudio::LmStudioBackend::default();
        assert!(backend.auth_headers("").is_empty());
    }

    #[test]
    fn lmstudio_uses_bearer_when_key_provided() {
        let backend = lmstudio::LmStudioBackend::default();
        let headers = backend.auth_headers("mytoken");
        assert_eq!(headers[0].0, "Authorization");
        assert_eq!(headers[0].1, "Bearer mytoken");
    }

    #[test]
    fn select_backend_openai_compat() {
        let b = select_backend("openai_compat");
        assert_eq!(b.endpoint_path(), "/chat/completions");
    }

    #[test]
    fn select_backend_default_is_mimo() {
        let b = select_backend("");
        assert_eq!(b.endpoint_path(), "/chat/completions");
    }

    // ── Timeout defaults ──────────────────────────────────────────────

    #[test]
    fn local_backends_have_longer_timeout() {
        let ollama = select_backend("ollama");
        let lmstudio = select_backend("lmstudio");
        let mimo = select_backend("mimo");

        assert!(ollama.default_timeout_secs() >= 300);
        assert!(lmstudio.default_timeout_secs() >= 300);
        assert!(mimo.default_timeout_secs() < 300); // cloud is faster
    }
}
