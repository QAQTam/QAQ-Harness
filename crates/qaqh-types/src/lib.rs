// ── Type definitions for qaqh core types ──
//
// All type definitions are split across sub-modules below.
// This file re-exports every public symbol so consumers can
// use `qaqh_types::TypeName` without caring about sub-module layout.

// ── Sub-module declarations (each file = one logical group) ──

pub mod api_types;
pub mod config;
pub mod message;
pub mod provider;
pub mod session;
pub mod state;
pub mod tool_def;
pub mod tool_mode;
pub mod tool_result;

// Unified arg parsing (shared across dsx-agent, dsx-tools)
pub mod arg;

// Platform-specific utilities
pub mod platform;

pub mod token;

// ── Re-exports: flat public API ──

pub use api_types::UsageInfo;
pub use config::{
    BalanceInfo, ConfigStore, PersistentConfig, PersistentMultimodalConfig,
    PersistentSubagentConfig, PersistentWorkspaceConfig, ProfileConfig,
};
pub use message::{ContentBlock, FunctionCall, Message, ToolCall};
pub use provider::{CacheTokenField, EndpointSpec, ProviderSpec, ThinkingParamMode, UserSendMode};
pub use session::{SessionMeta, SkillSessionEntry, SkillSessionEntryState, SkillSessionStateV2};
pub use state::DebugLevel;
pub use tool_def::{ToolDef, ToolFunction};
pub use tool_mode::{
    CUSTOM, KNOWN_MODES, MINIMAL, MINIMAL_B, MINIMAL_C, MINIMAL_DSH, MINIMAL_DSH_MODEL_TOOLS,
    MINIMAL_DSH_TOOLS, MINIMAL_PREFIX, MINIMAL_TOOLS, MINIMAL_TOOLS_B, MINIMAL_TOOLS_C, STANDARD,
    internal_tool_name, is_known, is_minimal_dsh, is_minimal_family, model_tool_name, preset_tools,
};
pub use tool_result::{
    ContentRef, TOOL_MODEL_MAX_CHARS, TOOL_SUMMARY_MAX_CHARS, ToolContinuation, ToolError,
    ToolModelPayload, ToolResult, ToolStatus,
};

// ── Unified arg parsers ──
pub use arg::{
    parse_arg, parse_arg_or, parse_cmd_arg, parse_file_arg, parse_opt, parse_opt_u64, tool_action,
};

// ── Shared utilities ──
pub use token::{TokenBreakdown, count_tokens, init_tokenizer};

// ── Product identity ──
pub use platform::{QAQH_UA_VERSION, QAQH_USER_AGENT};
