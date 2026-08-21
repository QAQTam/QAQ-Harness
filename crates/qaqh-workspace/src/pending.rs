//! dry-run 参数暂存注册表：`confirm_apply` 的内存直提基础。
//!
//! 写工具（edit / apply_patch / write）以 `dry_run=true` 通过验证后，
//! 把**重放用参数**（不含 dry_run）存进这里并返回 `pending_id`；模型确认后
//! 调 `confirm_apply { pending_id }` —— 引擎从注册表取出参数重放执行路径，
//! **模型不需要重新输出 patch / hunks / 内容**（消除二次输出）。
//!
//! - 一次性：`take` 即移除（apply 或 discard 都消费掉）；
//! - TTL：过期条目视为不存在（惰性清理，无后台任务）；
//! - 防 race：重放时各工具的 expected_hash 校验拦截 dry-run 之后发生的
//!   文件改动（dry-run 时读到的 hash 会写入暂存参数）。

use std::collections::HashMap;
use std::sync::LazyLock;
use std::sync::RwLock;
use std::time::{Duration, Instant};

/// pending 有效期：确认窗口 30 分钟（模型问用户 + 用户回复足够）。
const TTL: Duration = Duration::from_secs(30 * 60);

pub struct PendingApply {
    pub tool_name: String,
    /// 重放用参数（dry_run 已剥离；expected_hash 已注入 dry-run 时读到的值）。
    pub args: serde_json::Value,
    pub created_at: Instant,
}

static PENDING: LazyLock<RwLock<HashMap<String, PendingApply>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));
static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 暂存参数并返回短 pending_id（格式 `p<时间戳><序号>`，防猜）。
pub fn store(tool_name: &str, args: &serde_json::Value) -> String {
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let id = format!(
        "p{:x}{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0),
        seq
    );
    let mut map = PENDING.write().unwrap_or_else(|e| e.into_inner());
    map.insert(
        id.clone(),
        PendingApply {
            tool_name: tool_name.to_string(),
            args: args.clone(),
            created_at: Instant::now(),
        },
    );
    id
}

/// 取走 pending（一次性；过期或不存在 → None）。
pub fn take(id: &str) -> Option<PendingApply> {
    let mut map = PENDING.write().unwrap_or_else(|e| e.into_inner());
    match map.remove(id) {
        Some(p) if p.created_at.elapsed() <= TTL => Some(p),
        Some(_) => None,
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_take_roundtrip() {
        let id = store("edit", &serde_json::json!({"path": "a.rs"}));
        assert!(id.starts_with('p'));
        let p = take(&id).expect("pending exists");
        assert_eq!(p.tool_name, "edit");
        assert_eq!(p.args["path"], "a.rs");
        // 一次性：再取为空
        assert!(take(&id).is_none());
    }

    #[test]
    fn unknown_or_taken_id_is_none() {
        assert!(take("pnonexistent").is_none());
        let id = store("write", &serde_json::json!({}));
        take(&id).unwrap();
        assert!(take(&id).is_none());
    }
}
