//! Multimodal backend trait — abstraction over vision LLM providers.
//!
//! Each provider (MiMo, Ollama, LM Studio / OpenAI-compatible) implements
//! this trait so the image_query tool can dispatch to any backend uniformly.
//!
//! To add a new backend, implement this trait and register it in
//! `select_backend()` (mod.rs).

/// A multimodal (vision) backend that can accept an image + prompt and return text.
pub trait MultimodalBackend: Send + Sync {
    /// Build the JSON request body for this backend.
    ///
    /// * `model` — model name (e.g. "mimo-v2.5", "llava:13b")
    /// * `mime_type` — e.g. "image/png", "image/jpeg"
    /// * `base64_data` — raw base64 image data (no `data:` URI prefix)
    /// * `prompt` — what to ask about the image
    /// * `max_tokens` — max output tokens
    fn build_request(
        &self,
        model: &str,
        mime_type: &str,
        base64_data: &str,
        prompt: &str,
        max_tokens: u32,
    ) -> serde_json::Value;

    /// API endpoint path (appended to base_url).
    /// E.g. `"/v1/chat/completions"` or `"/api/chat"`.
    fn endpoint_path(&self) -> &'static str;

    /// Authentication headers. Return an empty Vec if no auth is needed.
    /// Each tuple is `(header_name, header_value)`.
    fn auth_headers(&self, api_key: &str) -> Vec<(String, String)>;

    /// Extract the assistant's text content from the response JSON.
    /// Returns `None` if the response structure is unexpected.
    fn extract_content<'a>(&self, json: &'a serde_json::Value) -> Option<&'a str>;

    /// Extract token usage from the response JSON.
    /// Returns `(prompt_tokens, completion_tokens)` or `None`.
    fn extract_usage(&self, json: &serde_json::Value) -> Option<(u64, u64)>;

    /// Maximum image size in bytes (for pre-flight validation).
    /// Default: 50 MB (MiMo limit).
    fn max_image_bytes(&self) -> usize {
        50 * 1024 * 1024
    }

    /// Default timeout in seconds.
    /// Local models need more time for inference.
    fn default_timeout_secs(&self) -> u64 {
        120
    }
}
