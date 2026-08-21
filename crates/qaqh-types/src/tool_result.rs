//! Canonical tool execution result shared by the tool runtime and Ringing.
//!
//! A tool has one authoritative status. Human summaries, compact metadata and
//! the bounded model projection are separate fields so transport and UI code
//! never have to infer failure from the shape of textual output.

use serde::{Deserialize, Serialize};

pub const TOOL_SUMMARY_MAX_CHARS: usize = 512;
// Keep the model projection near the planned six-thousand-token budget.
// The limit is expressed in Unicode characters because provider tokenizers
// are not available at this shared contract boundary.
pub const TOOL_MODEL_MAX_CHARS: usize = 24_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Ok,
    Error,
    Partial,
    Backgrounded,
    Cancelled,
}

impl ToolStatus {
    pub fn is_success(self) -> bool {
        matches!(self, Self::Ok | Self::Backgrounded)
    }

    pub fn is_failure(self) -> bool {
        matches!(self, Self::Error | Self::Partial | Self::Cancelled)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentRef {
    pub content_id: String,
    pub media_type: String,
    pub sha256: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolContinuation {
    pub tool: String,
    pub args: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolModelPayload {
    pub text: String,
    pub truncated: bool,
    pub total_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<ToolContinuation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub status: ToolStatus,
    pub summary: String,
    pub data: serde_json::Value,
    pub model: ToolModelPayload,
    /// 展示平面的 unified diff（文件修改类工具的原始 diff 文本）。
    ///
    /// ⚠ 绝不进入模型投影（`project_for_model` 不携带它）：模型看到的仍是
    /// 紧凑摘要行，diff 只供 timeline/前端抽屉消费。发送方在成功路径上
    /// 显式填充；缺失时默认 None（向后兼容）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_ref: Option<ContentRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ToolError>,
}

impl ToolResult {
    pub fn ok(text: impl Into<String>) -> Self {
        Self::ok_with_limit(text.into(), Some(TOOL_MODEL_MAX_CHARS))
    }

    /// Create a success result with an explicit model-text cap.
    ///
    /// `limit = None` disables the model projection cap entirely (used by
    /// no-fold / extreme modes where the model itself controls context);
    /// `Some(n)` bounds the projected text to `n` characters like [`Self::ok`].
    pub fn ok_with_limit(text: String, limit: Option<usize>) -> Self {
        Self::text_with_limit(ToolStatus::Ok, text, limit)
    }

    pub fn partial(text: impl Into<String>) -> Self {
        let text = text.into();
        Self::with_error(
            ToolStatus::Partial,
            text.clone(),
            "PARTIAL",
            text,
            false,
            None,
        )
    }

    pub fn cancelled(text: impl Into<String>) -> Self {
        let text = text.into();
        Self::with_error(
            ToolStatus::Cancelled,
            text.clone(),
            "CANCELLED",
            text,
            false,
            None,
        )
    }

    pub fn backgrounded(text: impl Into<String>) -> Self {
        Self::text(ToolStatus::Backgrounded, text.into())
    }

    pub fn error(message: impl Into<String>) -> Self {
        let message = message.into();
        Self::error_with("TOOL_ERROR", message, false, None)
    }

    pub fn error_with(
        code: impl Into<String>,
        message: impl Into<String>,
        retryable: bool,
        hint: Option<String>,
    ) -> Self {
        let code = code.into();
        let message = message.into();
        Self::with_error(
            ToolStatus::Error,
            message.clone(),
            code,
            message,
            retryable,
            hint,
        )
    }

    pub fn ok_data(data: serde_json::Value, text: impl Into<String>) -> Self {
        let text = text.into();
        let mut result = Self::text(ToolStatus::Ok, text);
        result.data = compact_data(data);
        result
    }

    pub fn error_data(
        code: impl Into<String>,
        message: impl Into<String>,
        retryable: bool,
        hint: Option<String>,
        data: serde_json::Value,
    ) -> Self {
        let mut result = Self::error_with(code, message, retryable, hint);
        result.data = compact_data(data);
        result
    }

    pub fn text(status: ToolStatus, text: String) -> Self {
        Self::text_with_limit(status, text, Some(TOOL_MODEL_MAX_CHARS))
    }

    fn text_with_limit(status: ToolStatus, text: String, model_limit: Option<usize>) -> Self {
        let model_text = match model_limit {
            Some(limit) => bounded_text(&text, limit),
            None => (text.clone(), false),
        };
        Self {
            status,
            summary: bounded_text(&text, TOOL_SUMMARY_MAX_CHARS).0,
            data: serde_json::Value::Object(Default::default()),
            model: ToolModelPayload {
                text: model_text.0,
                truncated: model_text.1,
                total_tokens: estimate_tokens(&text),
                continuation: None,
            },
            diff: None,
            output_ref: None,
            error: None,
        }
    }

    /// Attach display-plane diff text (never projected to the model).
    pub fn with_diff(mut self, diff: impl Into<String>) -> Self {
        self.diff = Some(diff.into());
        self
    }

    pub fn with_error(
        status: ToolStatus,
        summary: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
        retryable: bool,
        hint: Option<String>,
    ) -> Self {
        let mut result = Self::text(status, summary.into());
        result.error = Some(ToolError {
            code: code.into(),
            message: bounded_text(&message.into(), TOOL_SUMMARY_MAX_CHARS).0,
            retryable,
            hint: hint.map(|value| bounded_text(&value, TOOL_SUMMARY_MAX_CHARS).0),
        });
        result
    }

    pub fn is_success(&self) -> bool {
        self.status.is_success()
    }

    pub fn model_text(&self) -> &str {
        &self.model.text
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.summary.chars().count() > TOOL_SUMMARY_MAX_CHARS {
            return Err("summary exceeds the Unicode character budget");
        }
        if self.status.is_failure() && self.error.is_none() {
            return Err("failure result must include error");
        }
        if self.status.is_success() && self.error.is_some() {
            return Err("successful result must not include error");
        }
        // Model-text budget is policy-driven (ToolResult::ok_with_limit):
        // no-fold / extreme modes intentionally allow results beyond the
        // default TOOL_MODEL_MAX_CHARS, so no hard check is applied here.
        Ok(())
    }

    /// Stable payload used by provider adapters and context accounting.
    pub fn project_for_model(&self) -> serde_json::Value {
        serde_json::json!({
            "status": self.status,
            "summary": self.summary,
            "data": self.data,
            "text": self.model.text,
            "truncated": self.model.truncated,
            "continuation": self.model.continuation,
        })
    }
}

fn compact_data(data: serde_json::Value) -> serde_json::Value {
    match data {
        serde_json::Value::Object(mut object) => {
            object.remove("stdout");
            object.remove("stderr");
            object.remove("output");
            object.remove("content");
            serde_json::Value::Object(object)
        }
        serde_json::Value::Null => serde_json::Value::Object(Default::default()),
        other => other,
    }
}

fn bounded_text(text: &str, max_chars: usize) -> (String, bool) {
    let mut chars = text.chars();
    let bounded: String = chars.by_ref().take(max_chars).collect();
    let truncated = chars.next().is_some();
    (bounded, truncated)
}

fn estimate_tokens(text: &str) -> u64 {
    ((text.chars().count() as u64) + 3) / 4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_status_is_the_only_failure_authority() {
        let result =
            ToolResult::error_with("NOT_FOUND", "missing", false, Some("retry read".into()));
        assert_eq!(result.status, ToolStatus::Error);
        assert!(result.error.is_some());
        assert!(!result.is_success());
        result.validate().unwrap();
    }

    #[test]
    fn summary_budget_is_unicode_safe_and_model_projection_is_stable() {
        let result = ToolResult::ok("界".repeat(TOOL_MODEL_MAX_CHARS + 100));
        assert_eq!(result.summary.chars().count(), TOOL_SUMMARY_MAX_CHARS);
        assert!(result.model.truncated);
        assert!(result.project_for_model().get("success").is_none());
        result.validate().unwrap();
    }

    #[test]
    fn compact_data_drops_large_inline_output_fields() {
        let result = ToolResult::ok_data(
            serde_json::json!({"path":"a.rs", "stdout":"large", "output":"large"}),
            "done",
        );
        assert_eq!(result.data["path"], "a.rs");
        assert!(result.data.get("stdout").is_none());
        assert!(result.data.get("output").is_none());
    }

    #[test]
    fn ok_with_limit_none_preserves_full_text_and_validates() {
        // 极限模式（no-fold）：模型自己控制上下文，结果完整透传。
        let body = "x".repeat(TOOL_MODEL_MAX_CHARS * 4);
        let result = ToolResult::ok_with_limit(body.clone(), None);
        assert_eq!(result.model.text, body);
        assert!(!result.model.truncated);
        result.validate().unwrap();

        // Some(limit) 仍按预算截断（与 ok() 一致）。
        let capped = ToolResult::ok_with_limit(body, Some(100));
        assert_eq!(capped.model.text.chars().count(), 100);
        assert!(capped.model.truncated);
    }
}
