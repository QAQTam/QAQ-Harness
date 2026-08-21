use serde::{Deserialize, Serialize};

use crate::ToolResult;

// ── OpenAI-native content blocks ──

/// Content block within a message, matching OpenAI / DeepSeek Chat Completions API.
///
/// Messages use content blocks instead of flat strings to support mixed text +
/// tool call + tool result + reasoning content within a single turn.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ContentBlock {
    /// Plain text content from the model or user.
    #[serde(rename = "text")]
    Text { text: String },
    /// Model reasoning/thinking output (shown as collapsible in UI).
    /// Separate from `Text` so the frontend can style reasoning differently.
    #[serde(rename = "reasoning")]
    Reasoning { reasoning: String },
    /// A tool call the model wants to execute.
    /// Includes the tool name, input arguments, and a unique call ID for tracking.
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// Result of executing a previously requested tool call.
    /// Fed back to the model as context for the next inference step.
    #[serde(rename = "tool_result")]
    ToolResult {
        /// Matches the `id` from the corresponding `ToolUse` block.
        tool_use_id: String,
        /// Canonical structured result. `status` is the only execution truth.
        result: ToolResult,
    },
    /// An image for multimodal understanding (user messages only).
    /// `mime_type` is the MIME type (e.g. "image/png", "image/jpeg").
    /// `data` is the base64-encoded image data (without the `data:...;base64,` prefix).
    #[serde(rename = "image")]
    Image {
        /// MIME type of the image (e.g. "image/png", "image/jpeg").
        mime_type: String,
        /// Base64-encoded image data (raw, without the data URI prefix).
        data: String,
    },
    /// A server-side web search call emitted by the model (Responses API
    /// built-in tool). The search itself runs on the provider; this block is
    /// carried in history and echoed back verbatim on the next turn so the
    /// server restores its search results (stateless multi-turn).
    #[serde(rename = "web_search_call")]
    WebSearchCall {
        /// Output item id (e.g. "ws_1"); must be unique per response.
        id: String,
        /// The server-executed search action, e.g. {"type": "search"}.
        action: serde_json::Value,
    },
    /// Opaque output item returned by the Responses API.
    ///
    /// This block is protocol state rather than user-visible content. It keeps
    /// provider-owned fields such as Codex `phase` and reasoning
    /// `encrypted_content` intact across persistence and tool-loop replay.
    /// UI projections intentionally ignore it.
    #[serde(rename = "response_output_item")]
    ResponseOutputItem { item: serde_json::Value },
}

impl ContentBlock {
    /// Convenience constructor for a text content block.
    pub fn text(text: &str) -> Self {
        ContentBlock::Text {
            text: text.to_string(),
        }
    }

    /// Convenience constructor for an image content block.
    pub fn image(mime_type: &str, base64_data: &str) -> Self {
        ContentBlock::Image {
            mime_type: mime_type.to_string(),
            data: base64_data.to_string(),
        }
    }
}

// ── Messages ──

/// A conversation message using OpenAI-native content-block format.
///
/// Roles:
/// - `"user"` — contains `Text`, `Image`, and/or `ToolResult` blocks
/// - `"assistant"` — contains visible `Text`/`Reasoning`/`ToolUse` blocks and
///   may carry hidden `ResponseOutputItem` protocol state
/// - `"system"` — system-level context and instructions
/// - `"tool"` — tool execution results
///
/// The optional `name` field distinguishes same-role participants
/// (e.g. `name="docs"` for injected document context, `name="code"` for
/// code snippets). It maps to OpenAI's `name` parameter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// Monotonic per-session message ID for ordering and dedup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msg_id: Option<u64>,
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub content: Vec<ContentBlock>,
}

impl Message {
    /// Role constants — the four open roles of the context flow.
    pub const ROLE_SYSTEM: &'static str = "system";
    pub const ROLE_USER: &'static str = "user";
    pub const ROLE_ASSISTANT: &'static str = "assistant";
    pub const ROLE_TOOL: &'static str = "tool";
    /// Responses-protocol-only role for runtime-injected instructions
    /// (skills envelope, subagent reports, …). Downgrades to `system` on
    /// Chat Completions providers, maps to a `developer` item on Responses.
    pub const ROLE_DEVELOPER: &'static str = "developer";

    /// Create a system message with a single text block.
    pub fn system(content: &str) -> Self {
        Self {
            msg_id: None,
            role: Self::ROLE_SYSTEM.into(),
            name: None,
            content: vec![ContentBlock::text(content)],
        }
    }
    /// Create a developer message (Responses runtime-injection role). Falls
    /// back to `system` semantics on Chat Completions providers — the
    /// downgrade happens at the gate conversion layer, not at storage.
    pub fn developer(content: &str) -> Self {
        Self {
            msg_id: None,
            role: Self::ROLE_DEVELOPER.into(),
            name: None,
            content: vec![ContentBlock::text(content)],
        }
    }
    /// Create a user message with a single text block.
    pub fn user(content: &str) -> Self {
        Self {
            msg_id: None,
            role: Self::ROLE_USER.into(),
            name: None,
            content: vec![ContentBlock::text(content)],
        }
    }
    /// Create a tool result message, feeding tool output back to the model.
    pub fn tool(tool_call_id: &str, result: &str, success: bool) -> Self {
        let result = if success {
            ToolResult::ok(result)
        } else {
            ToolResult::error(result)
        };
        Self::tool_result(tool_call_id, result)
    }

    /// Create a tool result message with the canonical structured result.
    pub fn tool_result(tool_call_id: &str, result: ToolResult) -> Self {
        Self {
            msg_id: None,
            role: Self::ROLE_TOOL.into(),
            name: None,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: tool_call_id.into(),
                result,
            }],
        }
    }
}

// ── Tool Call (kept for IPC, XML/DSML parsing, and backward compat) ──

/// A tool call invocation, used in JSON-based tool call protocols.
///
/// Note: new code prefers `ContentBlock::ToolUse` for OpenAI-native format.
/// `ToolCall` remains for XML/DSML parsing and backward compatibility.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Unique call identifier for tracking and result matching.
    pub id: String,
    /// Always `"function"` for function-call-style tools.
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: FunctionCall,
}

/// The function details within a tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    /// Name of the tool to invoke (e.g. "read", "exec").
    pub name: String,
    /// JSON-encoded arguments string.
    pub arguments: String,
}
