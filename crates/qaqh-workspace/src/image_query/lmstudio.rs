//! LM Studio native multimodal backend.
//!
//! LM Studio's native REST API (`/api/v1/chat`) uses a completely different
//! format from OpenAI Chat Completions — `"input"` array instead of
//! `"messages"`, `"data_url"` for images instead of `"image_url"`, and
//! `"output"` array instead of `"choices"` in the response.
//!
//! Default URL: `http://localhost:1234`
//! Auth: `Authorization: Bearer <token>` (optional for local use)
//! Vision models: `qwen/qwen3-vl-4b`, `llava-v1.6`, etc.

use super::backend::MultimodalBackend;

/// LM Studio native backend — uses `/api/v1/chat` (NOT OpenAI-compatible).
#[derive(Default)]
pub struct LmStudioBackend;

impl MultimodalBackend for LmStudioBackend {
    fn build_request(
        &self,
        model: &str,
        mime_type: &str,
        base64_data: &str,
        prompt: &str,
        _max_tokens: u32,
    ) -> serde_json::Value {
        serde_json::json!({
            "model": model,
            "input": [
                {
                    "type": "text",
                    "content": prompt
                },
                {
                    "type": "image",
                    "data_url": format!("data:{};base64,{}", mime_type, base64_data)
                }
            ],
            "context_length": 4096,
            "temperature": 0.0,
            // Explicitly disable thinking/reasoning — we only want the final answer
            "reasoning": { "type": "disabled" }
        })
    }

    fn endpoint_path(&self) -> &'static str {
        "/api/v1/chat"
    }

    fn auth_headers(&self, api_key: &str) -> Vec<(String, String)> {
        if api_key.is_empty() {
            vec![] // LM Studio local has no auth by default
        } else {
            vec![("Authorization".into(), format!("Bearer {}", api_key))]
        }
    }

    fn extract_content<'a>(&self, json: &'a serde_json::Value) -> Option<&'a str> {
        // LM Studio output is an array of {type, content} objects.
        // Types: "message", "reasoning", "tool_call".
        // We want the LAST "message" type (final answer, not reasoning).
        let output = json.get("output")?.as_array()?;
        output
            .iter()
            .rev() // search backwards so we get the final message
            .find(|item| item.get("type").and_then(|t| t.as_str()) == Some("message"))
            .and_then(|item| item.get("content")?.as_str())
    }

    fn extract_usage(&self, json: &serde_json::Value) -> Option<(u64, u64)> {
        // LM Studio: stats.input_tokens / stats.total_output_tokens
        let stats = json.get("stats")?;
        let prompt = stats.get("input_tokens")?.as_u64()?;
        let completion = stats.get("total_output_tokens")?.as_u64()?;
        Some((prompt, completion))
    }

    fn default_timeout_secs(&self) -> u64 {
        300
    }
}
