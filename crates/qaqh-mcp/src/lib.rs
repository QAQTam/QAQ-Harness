//! qaqh-mcp：QAQ-Harness 的 MCP 客户端支持。
//!
//! 设计文档：`docs/mcp-client-design.md`（D1–D6 决策 + §5 核心机制）。
//! 实施计划：`docs/PLAN.md`（分阶段 PR 与出口命令）。
//!
//! ## M1-2（PR-M1-2）已落地
//!
//! - [`manager::McpManager`]：daemon 级生命周期容器（lazy connect / 冷却 /
//!   `shutting_down` 闸 / RAII）；全局槽位 [`manager_slot`] +
//!   [`install_manager`]（OnceLock 模式，归属 QaqhService 装配，设计 §10-6）。
//! - [`connection::ServerConnection`]：per-server 连接状态机
//!   （`inflight==0` idle 回收、crash 标记、5s 重连冷却）。
//! - [`adapter`]（crate 内部）：rmcp/process-wrap 隔离 + 进程组组装。
//!
//! M1-3（secrets 插值）/ M1-4（工具投影与动态注册）/ M1-5（桥接与调用路径）
//! 陆续接入；本 crate 不反向依赖 runtime/msgloop（设计 §4 依赖方向）。
//!
//! ## SDK 隔离边界
//!
//! 设计 §4 要求"SDK 类型不出 crate"。[`bridge`] 是模型面/ToolResult 的隔离
//! 收口层（CallToolResult/ServiceError 不出 [`bridge`] 与 [`adapter`]）；
//! [`connection::ConnectFactory`] 等内部管线类型仍暴露 rmcp 的
//! `RunningService`（集成测试与桥接需要），升级 SDK 只改 [`adapter`]。

pub mod bridge;
pub mod connection;
pub mod error;
pub mod manager;
pub mod projection;
pub mod resources;
pub mod sanitize;

// PR-M2-2：NotifyBridge 经 ClientService 别名穿透到测试 mock factory——
// doc(hidden) 保持“SDK 隔离边界”精神（非公共契约，升级可能变动）。
#[doc(hidden)]
pub mod adapter;

pub use bridge::{
    dispatch, prime_all_async, runtime_handle, shutdown_global, take_projection_batch,
};
pub use connection::{
    CallGuard, ClientService, ConnStatus, ConnectFactory, ConnectFuture, LifecycleSettings,
    ServerConnection,
};
pub use error::{McpError, McpErrorKind};
pub use manager::McpManager;
pub use resources::resource_env_block;
pub use sanitize::redact_secrets;

/// 集成测试垫片（doc(hidden)）：把桥接/投影的可测形态暴露给
/// `tests/call_path.rs` 等集成测试——绕开全局槽位，用例可并行。
/// 非公共 API 契约，升级可能变动；生产代码不得使用。
#[doc(hidden)]
pub mod bridge_for_tests {
    use crate::McpManager;

    #[doc(hidden)]
    pub use crate::bridge::{dispatch_with, projection_batch_with};
    #[doc(hidden)]
    pub use crate::resources::{aggregate_dispatch_with, resource_env_block_with};

    /// 测试垫片：手动置脏（零 server 配置时无连接可触发）。
    #[doc(hidden)]
    pub fn mark_dirty(manager: &McpManager) {
        manager.mark_dirty();
    }
}

/// M0 起保留的 crate 用途标识。
pub const CRATE_PURPOSE: &str = "QAQ-Harness MCP client support — see docs/mcp-client-design.md";

use std::sync::{Arc, OnceLock, RwLock};

fn manager_slot_raw() -> &'static RwLock<Arc<McpManager>> {
    static MANAGER_SLOT: OnceLock<RwLock<Arc<McpManager>>> = OnceLock::new();
    // 槽位模式范本：backend.rs:227（OnceLock + 读写锁中毒 into_inner）。
    MANAGER_SLOT.get_or_init(|| RwLock::new(McpManager::disabled()))
}

/// 当前全局 [`McpManager`]（未装配时为 disabled 占位：所有调用报 `MCP_DISABLED`）。
pub fn manager_slot() -> Arc<McpManager> {
    manager_slot_raw()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// 装配全局 manager，返回上一个（调用方负责其关闭——QaqhService 装配点
/// 一次性调用；P2-1 热重载路径经 [`manager_slot`] 读取，manager 单例不变）。
pub fn install_manager(manager: Arc<McpManager>) -> Arc<McpManager> {
    let mut slot = manager_slot_raw()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    std::mem::replace(&mut *slot, manager)
}
