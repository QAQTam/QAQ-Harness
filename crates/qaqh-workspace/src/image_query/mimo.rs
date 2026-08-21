//! MiMo (Xiaomi) multimodal backend.
//!
//! Uses `api-key` header authentication and OpenAI-compatible Chat Completions API.
//! Endpoint: `POST https://api.xiaomimimo.com/v1/chat/completions`

use super::backend::MultimodalBackend;

/// MiMo backend — Xiaomi's vision model (mimo-v2.5).
#[derive(Default)]
pub struct MiMoBackend;

impl MultimodalBackend for MiMoBackend {
    fn build_request(
        &self,
        model: &str,
        mime_type: &str,
        base64_data: &str,
        prompt: &str,
        max_tokens: u32,
    ) -> serde_json::Value {
        serde_json::json!({
            "model": model,
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "image_url",
                            "image_url": {
                                "url": format!("data:{};base64,{}", mime_type, base64_data)
                            }
                        },
                        {
                            "type": "text",
                            "text": prompt
                        }
                    ]
                }
            ],
            "max_completion_tokens": max_tokens
        })
    }

    fn endpoint_path(&self) -> &'static str {
        "/chat/completions"
    }

    fn auth_headers(&self, api_key: &str) -> Vec<(String, String)> {
        vec![("api-key".into(), api_key.to_string())]
    }

    fn extract_content<'a>(&self, json: &'a serde_json::Value) -> Option<&'a str> {
        json.get("choices")?
            .get(0)?
            .get("message")?
            .get("content")?
            .as_str()
    }

    fn extract_usage(&self, json: &serde_json::Value) -> Option<(u64, u64)> {
        let usage = json.get("usage")?;
        let prompt = usage.get("prompt_tokens")?.as_u64()?;
        let completion = usage.get("completion_tokens")?.as_u64()?;
        Some((prompt, completion))
    }

    fn max_image_bytes(&self) -> usize {
        50 * 1024 * 1024 // MiMo limit
    }

    fn default_timeout_secs(&self) -> u64 {
        120
    }
}
