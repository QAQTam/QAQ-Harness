//! 进程内配置变更广播（PLAN P2-D1）。
//!
//! `Config::update` 单写口成功落盘后向订阅者推送最新快照；引擎/服务层
//! 订阅即可获得「整体不可变配置」，从根上消灭逐字段手抄漏同步类 bug
//! （2026-08-25 设置页事故 R1 的结构性根治面）。
//!
//! 选型：`tokio::sync::watch`（工作区标准）；载荷 `Arc<Config>` 克隆廉价、
//! 消费者拿到后零成本共享。当前消费方：
//! - `reload_config` 的 `latest()` 快速路径（免磁盘重读）；
//! - T18 ringing `config_changed` 推送事件；
//! - T20 axum `GET /api/config/events`(SSE)。

use std::sync::{Arc, OnceLock};

use tokio::sync::watch;

use crate::config::Config;

static CHANNEL: OnceLock<watch::Sender<Option<Arc<Config>>>> = OnceLock::new();
/// 最新快照镜像：`watch::send` 在零接收者时会丢弃载荷（tokio 语义），
/// 镜像槽保证「晚到的订阅者/读取者」也能拿到最近一次发布值。
static LATEST: OnceLock<std::sync::Mutex<Option<Arc<Config>>>> = OnceLock::new();

fn channel() -> &'static watch::Sender<Option<Arc<Config>>> {
    CHANNEL.get_or_init(|| watch::channel(None).0)
}

fn latest_slot() -> &'static std::sync::Mutex<Option<Arc<Config>>> {
    LATEST.get_or_init(|| std::sync::Mutex::new(None))
}

/// 订阅配置变更。返回的 Receiver 同时充当"最新值"读取口
/// （`borrow()` 拿 `Option<Arc<Config>>`，None = 尚无任何 update 发生）。
pub fn subscribe() -> watch::Receiver<Option<Arc<Config>>> {
    channel().subscribe()
}

/// 最新快照（若已有过至少一次单写口提交）。磁盘兜底由调用方自理：
/// daemon 启动早期尚无 update 时应走 `Config::load()`。
pub fn latest() -> Option<Arc<Config>> {
    latest_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// 单写口内部：发布新快照。仅 `Config::update` 成功路径调用，
/// 保证「磁盘已落盘 → 内存广播」顺序（消费者永远读到已持久化状态）。
pub(crate) fn publish(cfg: Arc<Config>) {
    *latest_slot().lock().unwrap_or_else(|e| e.into_inner()) = Some(cfg.clone());
    // 零接收者时 send 失败无妨：latest() 镜像已兜底。
    let _ = channel().send(Some(cfg));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 推送路径：先订阅后发布，receiver 立刻可见。
    #[test]
    fn push_path_receiver_sees_update() {
        let rx = subscribe();
        publish(Arc::new(Config {
            context_limit: 123_456,
            ..Default::default()
        }));
        assert_eq!(
            rx.borrow().clone().expect("pushed snapshot").context_limit,
            123_456
        );
    }

    /// 镜像路径：零接收者时发布不丢值，晚到的读取者经 latest() 兜底
    /// （tokio watch 零接收者 send 会丢弃载荷——2026-08-25 测试实证）。
    #[test]
    fn mirror_survives_zero_receiver_publish() {
        // 不持有任何 receiver，直接发布。
        publish(Arc::new(Config {
            context_limit: 654_321,
            ..Default::default()
        }));
        assert_eq!(latest().expect("mirror snapshot").context_limit, 654_321);
    }
}
