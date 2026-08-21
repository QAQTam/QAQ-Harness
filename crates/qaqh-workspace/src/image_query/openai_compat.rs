//! OpenAI-compatible multimodal backend.
//!
//! Covers LM Studio, vLLM, Ollama with OpenAI-compatible endpoints, and any
//! other server that speaks the OpenAI Chat Completions protocol.
//! Uses `Authorization: Bearer <key>` header (omitted when api_key is empty).

use super::backend::MultimodalBackend;

/// OpenAI-compatible backend — LM Studio, vLLM, generic OpenAI-protocol servers.
#[derive(Default)]
pub struct OpenAiCompatBackend;

impl MultimodalBackend for OpenAiCompatBackend {
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
            "max_tokens": max_tokens
        })
    }

    fn endpoint_path(&self) -> &'static str {
        "/chat/completions"
    }

    fn auth_headers(&self, api_key: &str) -> Vec<(String, String)> {
        if api_key.is_empty() {
            vec![] // LM Studio local typically has no auth
        } else {
            vec![("Authorization".into(), format!("Bearer {}", api_key))]
        }
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

    fn default_timeout_secs(&self) -> u64 {
        300 // Local models need more time for inference
    }
}
