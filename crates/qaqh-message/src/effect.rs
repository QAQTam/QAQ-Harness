use qaqh_types::Message;

/// Returned by MessageStore::push_* methods.
/// Tells external actors (runner) what to do next.
#[derive(Debug, Clone)]
pub enum Effect {
    /// No side effect.
    None,
    /// Call the gate with this context.
    CallGate { messages: Vec<Message> },
    /// Turn finished — save snapshot, return to idle.
    TurnComplete,
}

/// A tool invocation extracted from the assistant message.
#[derive(Debug, Clone)]
pub struct PendingTool {
    pub id: String,
    pub name: String,
    pub args: serde_json::Value,
}

/// Simplified tool execution request (sent from MessageStore to ToolManager).
#[derive(Debug, Clone)]
pub struct ToolExecRequest {
    pub id: String,
    pub name: String,
    pub args: serde_json::Value,
}

/// Simplified tool execution report (ToolManager → MessageStore).
#[derive(Debug, Clone)]
pub struct ToolExecReport {
    pub content: String,
    pub success: bool,
    /// Files affected by this tool call.
    pub files_affected: Vec<String>,
}

/// Callback type for tool execution.
pub type ToolExecutorFn = Box<dyn Fn(ToolExecRequest) -> ToolExecReport + Send>;
