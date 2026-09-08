//! Canonical tool execution result shared by the tool runtime and Ringing.
//!
//! A tool has one authoritative status. Human summaries, compact metadata and
//! the bounded model projection are separate fields so transport and UI code
//! never have to infer failure from the shape of textual output.

use serde::{Deserialize, Serialize};
#[cfg(feature = "ts")]
use ts_rs::TS;

pub const TOOL_SUMMARY_MAX_CHARS: usize = 512;
// Keep the model projection near the planned six-thousand-token budget.
// The limit is expressed in Unicode characters because provider tokenizers
// are not available at this shared contract boundary.
pub const TOOL_MODEL_MAX_CHARS: usize = 24_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
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
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct ContentRef {
    pub content_id: String,
    pub media_type: String,
    pub sha256: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct ToolContinuation {
    pub tool: String,
    pub args: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct ToolModelPayload {
    pub text: String,
    pub truncated: bool,
    #[cfg_attr(feature = "ts", ts(as = "u32"))]
    pub total_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<ToolContinuation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct ToolError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// An image attachment carried by a tool result.
///
/// Stored alongside [`ToolResult`] so the message layer can append
/// `ContentBlock::Image` blocks to the resulting tool message. Never
/// serialized into the model text projection — the gate lowers images
/// to provider-native media parts at request-build time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct ToolImage {
    pub mime_type: String,
    /// Raw base64 payload (no `data:` prefix).
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct ToolResult {
    pub status: ToolStatus,
    pub data: serde_json::Value,
    /// Images to attach to the tool result message (e.g. `read_image`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ToolImage>,
    /// 展示平面的 unified diff（文件修改类工具的原始 diff 文本）。
    ///
    /// ⚠ 绝不进入模型投影（`project_for_model` 不携带它）：模型看到的仍是
    /// 紧凑摘要行，diff 只供 timeline/前端抽屉消费。发送方在成功路径上
    /// 显式填充；缺失时默认 None（向后兼容）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ToolError>,

    // ── 投影区（私有）──────────────────────────────────────────────
    //
    // 以下三项是同一份工具输出的不同投影，历史上是 `pub` 字段，任何调用方都
    // 能单独改写其中一个而让另外两个失真（实例：`externalize_large_content`
    // 改 model.text 却让 summary 停留在原文；`amend_synthetic_repair` 改
    // model.text 而 summary 仍是占位符）。现收为私有，一律经本模块的构造函数
    // 或受控改写接口整体更新，**漂移因此变成编译错误**。
    //
    // serde 仍会序列化这三项：线上 JSON 与 ts-rs 生成的 TS 契约保持不变。
    //
    // `summary` 并非纯派生——部分工具会往里追加提示行（见 [`Self::push_hint`]）。
    /// 展示/模型提示行（≤ [`TOOL_SUMMARY_MAX_CHARS`]），可被工具追加。
    summary: String,
    /// 模型可见投影（文本、截断标记、token 估算、续调参数）。
    model: ToolModelPayload,
    /// 大输出外置引用。**仅在 NoFold 极限模式且模型文本超过
    /// `CONTENT_STORE_THRESHOLD_BYTES`（10 MiB）时才会产生**——标准模式下
    /// 模型文本已被 [`TOOL_MODEL_MAX_CHARS`] 封顶（≤ 约 96 KiB），永远到不了
    /// 该阈值。因此它是传输保护阀，而非常规路径：前端应视为可选字段，
    /// 不要指望靠它拿"完整输出"。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output_ref: Option<ContentRef>,
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
            images: Vec::new(),
        }
    }

    /// Attach display-plane diff text (never projected to the model).
    pub fn with_diff(mut self, diff: impl Into<String>) -> Self {
        self.diff = Some(diff.into());
        self
    }

    /// Attach an image (mime + base64) to be appended to the tool message.
    pub fn with_image(mut self, mime_type: impl Into<String>, data: impl Into<String>) -> Self {
        self.images.push(ToolImage {
            mime_type: mime_type.into(),
            data: data.into(),
        });
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

    /// 展示/模型提示行（`summary` 投影，只读）。
    pub fn summary(&self) -> &str {
        &self.summary
    }

    /// 模型投影是否被截断。
    pub fn model_truncated(&self) -> bool {
        self.model.truncated
    }

    /// 大输出外置引用（通常见 [`Self::output_ref`] 字段说明：极少产生）。
    pub fn output_ref(&self) -> Option<&ContentRef> {
        self.output_ref.as_ref()
    }

    /// 往提示行追加一行（如 `pending_id=… confirm with confirm_apply …`），
    /// 并**重新按 [`TOOL_SUMMARY_MAX_CHARS`] 收敛**。
    ///
    /// 直接 `push_str` 会让 summary 突破 512 上限（历史上 `apply_patch` /
    /// `edit` 的 dry-run 分支正是这么写的，而 `validate()` 在生产路径上从不
    /// 被调用，于是契约被静默违反）。走这个方法就不会。
    pub fn push_hint(&mut self, hint: &str) {
        self.summary.push('\n');
        self.summary.push_str(hint);
        self.summary = bounded_text(&self.summary, TOOL_SUMMARY_MAX_CHARS).0;
    }

    /// 整体替换模型文本，并重算 summary 与 token 估算。
    ///
    /// 用于"占位符 → 真实内容"的修复（如 `amend_synthetic_repair`）：这类场景
    /// 若只改模型文本，summary 会停留在占位符上，正是投影漂移的典型形态。
    /// `truncated` 与 `output_ref` 不在本方法职责内，保持不变。
    pub fn rewrite_text(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.summary = bounded_text(&text, TOOL_SUMMARY_MAX_CHARS).0;
        self.model.text = text;
        self.model.total_tokens = estimate_tokens(&self.model.text);
    }

    /// 套用工具侧折叠结果：替换模型文本并标记截断。
    ///
    /// 供 `tool_side_fold` 使用。summary **不随之改变**——它是全文的前 512
    /// 字符，而折叠只保留头部（命令输出限额 8K 远大于 512），两者天然一致。
    pub fn set_model_projection(&mut self, text: String, truncated: bool) {
        self.model.text = text;
        self.model.truncated = truncated;
        self.model.total_tokens = estimate_tokens(&self.model.text);
    }

    /// 大输出外置：一次性改写模型文本、截断标记、summary 与外置引用。
    ///
    /// 四项必须同时改写——历史上逐字段赋值导致 summary 与 `model.text` 语义
    /// 错乱（那时 summary 取的是 tail 的前 512 字符，等于输出中段，作为展示
    /// 行毫无意义）。`summary` 因此单独入参，由调用方给出**全文开头**。
    pub fn externalize_output(&mut self, retained: String, summary: String, reference: ContentRef) {
        self.summary = bounded_text(&summary, TOOL_SUMMARY_MAX_CHARS).0;
        self.model.text = retained;
        self.model.truncated = true;
        self.model.total_tokens = estimate_tokens(&self.model.text);
        self.output_ref = Some(reference);
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

    /// 统一 XML 信封：发给模型的 tool result 文本形态。
    ///
    /// 设计要点：
    /// - 属性区承载全部信令（status/truncated/error_code/retryable），
    ///   正文零转义省 token（替代旧 JSON to_string 的 \n 转义税）；
    /// - 标签名纯字母数字（<qaqh_tool_result>），避开 <|...|> special-token
    ///   命名空间（ChatML/Llama/DeepSeek 全家都占用了竖线形态）；
    /// - 正文中字面闭合标签替换为 <\/...> 反斜杠变体防嵌套假闭合。
    pub fn render_xml_envelope(&self) -> String {
        let status = match self.status {
            ToolStatus::Ok => "ok",
            ToolStatus::Error => "error",
            ToolStatus::Partial => "partial",
            ToolStatus::Backgrounded => "backgrounded",
            ToolStatus::Cancelled => "cancelled",
        };
        let mut out = format!("<qaqh_tool_result status=\"{status}\"");
        if self.model.truncated {
            out.push_str(" truncated=\"true\"");
        }
        if let Some(error) = &self.error {
            out.push_str(&format!(" error_code=\"{}\"", error.code));
            if error.retryable {
                out.push_str(" retryable=\"true\"");
            }
        }
        out.push_str(">\n");
        let mut body = self.model.text.trim_end().to_string();
        if body.is_empty()
            && let Some(error) = &self.error
        {
            body = error.message.clone();
        }
        let body = body.replace("</qaqh_tool_result>", "<\\/qaqh_tool_result>");
        out.push_str(&body);
        out.push_str("\n</qaqh_tool_result>");
        out
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
    (text.chars().count() as u64).div_ceil(4)
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
