//! 子代理宿主直连接口（Knife-1 step-2 收尾）。
//!
//! 主 session 与子代理 loop 均已 in-process（PR #17/#19/#21），工具 handler
//! 就在 daemon 进程内执行，因此 `spawn_subagent` 无需再经 daemon HTTP/SSE
//! 回连自己——直接调用宿主（daemon 进程内 AgentRegistry + RingingHub +
//! SessionManager）提供的 actor 句柄即可。
//!
//! 本 trait 只依赖 `qaqh_client` 已 re-export 的 wire 类型（`RingingCommand`、
//! `EventBatch` 等），不引用 qaqh-runtime 任何类型，保证依赖方向
//! `runtime → msgloop → subagent` 不回环。
//!
//! 安装：daemon 装配（`QaqhService::init`）时调用 [`install_host`]；工具
//! handler 通过 [`host`] 探测。宿主不可用时（如单元测试 / 非 daemon 进程）
//! 回退旧 HTTP/SSE 路径，保持行为兼容。

use std::sync::{Arc, Mutex, OnceLock};

use qaqh_client::RingingCommand;

/// 大内容引用与事件批次（宿主实现 `download_content` / 事件流需要；与 trait
/// 签名同类型），re-export 供 qaqh-runtime 消费。
pub use qaqh_client::{ContentRef, EventBatch};

/// 进程内子代理宿主演进接口。所有方法都是同步阻塞语义（与工具 worker
/// 线程的 std 线程模型匹配），由 qaqh-runtime 的 `QaqhService` 提供实现。
pub trait SubagentHost: Send + Sync {
    /// 生成新 seed 并在宿主进程内注册一个 in-process 子代理 actor。
    ///
    /// 与 daemon `subagent.spawn` action 等价：继承 workspace（写入
    /// SessionMeta.cwd）、应用 subagent 工具白名单。返回生成的 seed。
    fn spawn_subagent(
        &self,
        tools: &[String],
        model: Option<&str>,
        base_url: Option<&str>,
        max_tokens: Option<u32>,
        workspace: Option<&str>,
    ) -> Result<String, String>;

    /// 进程内直接向指定 seed 的 actor 命令队列发送一条 Ringing 命令
    /// （等价 HTTP attach + send_command，但进程内无 lease/owns 语义）。
    fn send_ringing(&self, seed: &str, command: RingingCommand) -> Result<(), String>;

    /// 订阅某 seed 的实时事件批次流（等价 SSE 单条连接；宿主内部按 seed
    /// 过滤后以 `EventBatch` 聚合）。返回 std mpsc receiver，供 std 线程消费。
    fn subscribe(&self, seed: &str) -> std::sync::mpsc::Receiver<EventBatch>;

    /// 进程内读取外置大内容（等价 HTTP `download_content`）。
    fn download_content(&self, seed: &str, reference: &ContentRef) -> Result<Vec<u8>, String>;

    /// 进程内关闭子代理 worker（等价 HTTP `SessionClose`）。
    fn close(&self, seed: &str) -> Result<(), String>;
}

/// 进程级宿主安装位。多个 actor 并发调用 `host()` 读取；daemon 只安装一次。
static HOST: OnceLock<Mutex<Option<Arc<dyn SubagentHost>>>> = OnceLock::new();

/// 安装进程内宿主。重复安装被忽略（保留首次）。
pub fn install_host(host: Arc<dyn SubagentHost>) {
    let slot = HOST.get_or_init(|| Mutex::new(None));
    let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Some(host);
        log::info!("[SUBAGENT] in-process subagent host installed");
    }
}

/// 读取已安装的宿主；未安装返回 `None`（调用方回退 HTTP/SSE 路径）。
pub fn host() -> Option<Arc<dyn SubagentHost>> {
    let slot = HOST.get()?;
    let guard = slot.lock().ok()?;
    guard.clone()
}

/// 测试：清空宿主（仅测试路径；生产仅 `install_host` 一次）。
#[cfg(test)]
pub(crate) fn clear_host_for_test() {
    if let Some(slot) = HOST.get() {
        if let Ok(mut guard) = slot.lock() {
            *guard = None;
        }
    }
}
