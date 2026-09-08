//! 同步工具环 ↔ LSP 连接的桥接（mcp bridge.rs 同款：专属 runtime + 投影批次）。
//!
//! - E-5 单一 dispatcher：[`dispatch`] 是 `lsp` 聚合工具的 fn 指针；
//! - 专属 runtime：工具裸线程无 ambient runtime，`block_on` 专属 runtime
//!   驱动 async-lsp 会话（mcp RUNTIME_WORKERS=2 同款）；
//! - 投影批次：enabled 即在场（`lsp list_servers` 零配置可答"无配置"，mcp 钉底
//!   同款）；无连接维度——聚合工具不随 server 增减。

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use qaqh_workspace::{DynamicTool, ToolCallCtx, ToolResult};

use crate::manager::LspManager;
use crate::manager_slot;

/// ctx 超时缺省（LSP 请求轻量，30s；mcp 为 60s）。
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
const RUNTIME_WORKERS: usize = 2;

fn dedicated_runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(RUNTIME_WORKERS)
            .enable_all()
            .build()
            .expect("qaqh-lsp dedicated runtime must build")
    })
}

/// qaqh-lsp 专属 runtime 句柄（惰性构建，进程级单例）。
pub fn runtime_handle() -> &'static tokio::runtime::Handle {
    dedicated_runtime().handle()
}

/// daemon 退出路径的优雅关闭入口（只允许从非异步线程调用）。
pub fn shutdown_global() {
    let manager = manager_slot();
    runtime_handle().block_on(manager.shutdown_all());
}

/// 投影预热（fire-and-forget）：装配点调用一次，后台逐 server 连接。
///
/// mcp prime 同款鸡生蛋修复——但 `lsp` 聚合工具 enabled 即在场，预热只为
/// 加速首轮 definition（索引预热），不阻塞装配；未启用时 no-op。
pub fn prime_all_async(root: String) {
    let manager = manager_slot();
    if !manager.config().enabled {
        return;
    }
    runtime_handle().spawn(async move { manager.prime_all(&root).await });
}

/// E-5 单一 dispatcher：`lsp` 聚合工具的 fn 指针。
pub fn dispatch(ctx: ToolCallCtx) -> ToolResult {
    crate::tool::aggregate_dispatch(ctx)
}

/// 投影批次：enabled 即 Some（含聚合工具钉底），disabled 即 None。
pub fn take_projection_batch() -> Option<Vec<(String, DynamicTool)>> {
    projection_batch_with(&manager_slot())
}

/// [`take_projection_batch`] 的可测形态：manager 显式注入。
#[doc(hidden)]
pub fn projection_batch_with(manager: &Arc<LspManager>) -> Option<Vec<(String, DynamicTool)>> {
    if !manager.config().enabled {
        return None;
    }
    Some(vec![crate::tool::aggregate_entry(Duration::from_secs(
        DEFAULT_TIMEOUT_SECS,
    ))])
}
