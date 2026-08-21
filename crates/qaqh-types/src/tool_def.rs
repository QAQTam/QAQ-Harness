use serde::{Deserialize, Serialize};

/// OpenAI function-calling tool definition passed to the model.
///
/// Wraps a function name, description, and JSON Schema parameters.
/// Corresponds to the `tools` array in the OpenAI Chat Completions API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    /// Always `"function"` for function-call tools.
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: ToolFunction,
}

/// Schema for a single tool function.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFunction {
    /// Unique tool name the model uses to invoke this function.
    pub name: String,
    /// Human-readable description of what the tool does.
    /// Injected into the system prompt; should be precise.
    pub description: String,
    /// JSON Schema object describing the tool's parameters.
    pub parameters: serde_json::Value,
}
