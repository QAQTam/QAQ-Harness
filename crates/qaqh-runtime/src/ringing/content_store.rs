//! 大内容外置存储（PLAN 大内容外置）。
//!
//! - 工具完整输出、compact archive、超大 diff、诊断内容进入 content store；
//! - 事件只携带 `RingingContentRef`（content_id、media_type、bytes、sha256、truncated）；
//! - 客户端通过带鉴权的 HTTP GET 按需读取（range/分页）；
//! - content 设置会话所有权与生命周期；
//! - **API key、provider 原始响应和未脱敏错误禁止进入 content store**。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use qaqh_types::sha256_hex;

/// 超过该阈值的内容应外置（10 MiB）。
///
/// ⚠ 这是**传输保护阀**，而非常规路径。工具结果走向本 store 的唯一入口是
/// `registry::externalize_large_content`，在标准模式下模型文本已被
/// `TOOL_MODEL_MAX_CHARS`（24K 字符，上限约 96 KiB）封顶，永远够不到 10 MiB；
/// 只有 NoFold 极限模式下的超长输出才会触发。别把它当成"大输出的分页通道"。
///
/// 此外本 store 还服务于附件/图片（`attachment.rs`、`host_impl.rs`）与 HTTP
/// content 端点，那部分与工具结果外置无关，不受上述可达性限制。
pub const CONTENT_STORE_THRESHOLD_BYTES: usize = 10 * 1024 * 1024;

/// 默认生命周期（30 分钟）。
pub const DEFAULT_CONTENT_TTL: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone)]
pub struct ContentEntry {
    pub content_id: String,
    pub seed: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub truncated: bool,
    pub created_at: Instant,
    pub expires_at: Instant,
}

/// 大内容存储（有界、会话所有权、TTL 清理）。
#[derive(Debug, Default)]
pub struct ContentStore {
    entries: HashMap<String, ContentEntry>,
    max_entries: usize,
}

impl ContentStore {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            max_entries: 256,
        }
    }

    /// 存入内容。返回 content_id（SHA-256 前 32 hex 或随机）。
    pub fn put(&mut self, seed: &str, media_type: &str, bytes: Vec<u8>, truncated: bool) -> String {
        let content_id = sha256_hex(&bytes);
        let now = Instant::now();
        self.entries.insert(
            content_id.clone(),
            ContentEntry {
                content_id: content_id.clone(),
                seed: seed.to_string(),
                media_type: media_type.to_string(),
                sha256: content_id.clone(),
                bytes,
                truncated,
                created_at: now,
                expires_at: now + DEFAULT_CONTENT_TTL,
            },
        );
        while self.entries.len() > self.max_entries {
            // 淘汰最早过期条目
            let victim = self
                .entries
                .values()
                .min_by_key(|e| e.expires_at)
                .map(|e| e.content_id.clone())
                .expect("non-empty");
            self.entries.remove(&victim);
        }
        content_id
    }

    /// 读取（校验所有权）。过期条目惰性清理。
    pub fn get(&mut self, seed: &str, content_id: &str) -> Option<ContentEntry> {
        let entry = self.entries.get(content_id)?;
        if entry.seed != seed || entry.expires_at < Instant::now() {
            self.entries.remove(content_id);
            return None;
        }
        Some(entry.clone())
    }

    /// 会话关闭/切流时释放该会话内容。
    pub fn release_session(&mut self, seed: &str) -> usize {
        let before = self.entries.len();
        self.entries.retain(|_, e| e.seed != seed);
        before - self.entries.len()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// SHA-256 hex 由 `qaqh_types::sha256_hex` 单源提供（PR-4-2，审计 #6）。
// content_id 需跨进程稳定，两处实现曾逐字节同形；现仅存 types 一份，
// 严禁再复制构造（换实现/换 crate 必须两侧同步评估 content_id 兼容性）。
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_round_trip_with_ownership() {
        let mut store = ContentStore::new();
        let id = store.put("s1", "text/plain", vec![1, 2, 3], false);
        let entry = store.get("s1", &id).expect("owner can read");
        assert_eq!(entry.bytes, vec![1, 2, 3]);
        assert_eq!(entry.media_type, "text/plain");
        // 其他会话无权读取
        assert!(store.get("s2", &id).is_none());
        // 错误 id
        assert!(store.get("s1", "nope").is_none());
    }

    #[test]
    fn sha256_is_deterministic_and_distinct() {
        assert_eq!(sha256_hex(b"abc"), sha256_hex(b"abc"));
        assert_ne!(sha256_hex(b"abc"), sha256_hex(b"abd"));
        assert_eq!(sha256_hex(b"abc").len(), 64);
    }

    #[test]
    fn session_release_frees_content() {
        let mut store = ContentStore::new();
        let id = store.put("s1", "text/plain", vec![1], false);
        store.put("s2", "text/plain", vec![2], false);
        assert_eq!(store.release_session("s1"), 1);
        assert!(store.get("s1", &id).is_none());
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn expired_entry_is_evicted_lazily() {
        let mut store = ContentStore::new();
        let id = store.put("s1", "text/plain", vec![1], false);
        // 直接改过期时间
        store.entries.get_mut(&id).expect("exists").expires_at =
            Instant::now() - Duration::from_secs(1);
        assert!(store.get("s1", &id).is_none());
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn large_content_flagged_for_externalization() {
        // 阈值判定属于调用方策略；此处验证常量（编译期守卫）
        const _: () = assert!(CONTENT_STORE_THRESHOLD_BYTES >= 10 * 1024 * 1024);
        let mut store = ContentStore::new();
        let big = vec![0_u8; CONTENT_STORE_THRESHOLD_BYTES];
        let id = store.put("s1", "application/octet-stream", big, true);
        let entry = store.get("s1", &id).expect("big content readable");
        assert!(entry.truncated);
        assert_eq!(entry.bytes.len(), CONTENT_STORE_THRESHOLD_BYTES);
    }
}
