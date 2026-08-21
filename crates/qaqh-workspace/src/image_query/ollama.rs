//! Ollama multimodal backend.
//!
//! Ollama uses its own native `/api/chat` endpoint (NOT OpenAI-compatible).
//! Images are passed as a top-level `images` array of base64 strings
//! (without the `data:` URI prefix).

use super::backend::MultimodalBackend;

/// Ollama backend — local models via Ollama's native API.
#[derive(Default)]
pub struct OllamaBackend;

impl MultimodalBackend for OllamaBackend {
    fn build_request(
        &self,
        model: &str,
        _mime_type: &str,
        base64_data: &str,
        prompt: &str,
        _max_tokens: u32,
    ) -> serde_json::Value {
        // Ollama /api/chat format:
        // - images is a top-level array of base64 strings (no data URI prefix)
        // - stream: false to get a single response
        serde_json::json!({
            "model": model,
            "messages": [
                {
                    "role": "user",
                    "content": prompt,
                    "images": [base64_data]
                }
            ],
            "stream": false
        })
    }

    fn endpoint_path(&self) -> &'static str {
        "/api/chat"
    }

    fn auth_headers(&self, _api_key: &str) -> Vec<(String, String)> {
        vec![] // Ollama has no built-in auth
    }

    fn extract_content<'a>(&self, json: &'a serde_json::Value) -> Option<&'a str> {
        // Ollama response: { "message": { "content": "..." }, ... }
        json.get("message")?.get("content")?.as_str()
    }

    fn extract_usage(&self, json: &serde_json::Value) -> Option<(u64, u64)> {
        // Ollama provides token counts at top level
        let prompt = json.get("prompt_eval_count")?.as_u64()?;
        let completion = json.get("eval_count")?.as_u64()?;
        Some((prompt, completion))
    }

    fn default_timeout_secs(&self) -> u64 {
        300 // Local inference can be slow
    }
}
