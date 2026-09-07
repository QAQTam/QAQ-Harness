// ── Type definitions for qaqh core types ──
//
// All type definitions are split across sub-modules below.
// This file re-exports every public symbol so consumers can
// use `qaqh_types::TypeName` without caring about sub-module layout.

// ── Sub-module declarations (each file = one logical group) ──

pub mod api_types;
pub mod config;
pub mod discovery;
pub mod image_store;
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
    ConfigStore, PersistentConfig, PersistentMcpConfig, PersistentMcpServerConfig,
    PersistentSubagentConfig, PersistentWorkspaceConfig, ProfileConfig,
};
pub use discovery::{CONTROL_PROTOCOL_VERSION, DaemonDiscovery};
pub use image_store::sha256_hex;
pub use message::{ContentBlock, FunctionCall, Message, ToolCall};
pub use provider::{CacheTokenField, EndpointSpec, ProviderSpec, ThinkingParamMode, UserSendMode};
pub use session::{SessionMeta, SkillSessionEntry, SkillSessionEntryState, SkillSessionStateV2};
pub use state::DebugLevel;
pub use tool_def::{ToolDef, ToolFunction};
pub use tool_result::{
    ContentRef, TOOL_MODEL_MAX_CHARS, TOOL_SUMMARY_MAX_CHARS, ToolContinuation, ToolError,
    ToolImage, ToolModelPayload, ToolResult, ToolStatus,
};

// ── Unified arg parsers ──
pub use arg::{parse_arg, parse_arg_or, parse_opt};

// ── Shared utilities ──
pub use token::{count_tokens, init_tokenizer};

pub use tool_mode::{
    CUSTOM, KNOWN_MODES, MINIMAL, MINIMAL_B, MINIMAL_C, MINIMAL_PREFIX, MINIMAL_TOOLS,
    MINIMAL_TOOLS_B, MINIMAL_TOOLS_C, STANDARD, is_known, is_minimal_family, preset_tools,
};

// ── Product identity ──
pub use platform::{QAQH_UA_VERSION, QAQH_USER_AGENT};
