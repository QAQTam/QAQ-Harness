//! File state tracker — records all file operations for context injection.
//!
//! Generates a compact XML summary injected into the [Environment] block
//! at each turn, so the model always knows current file states without re-reading.
//!
//! Format: `<file_state>\n  path  200L  (edited)\n  ...\n</file_state>`

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::OnceLock;

use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

#[derive(Clone, Debug)]
struct FileEntry {
    op: &'static str,
    line_count: usize,
    /// 最近一次工具看到的内容指纹（**LF canonical 视图**，与 read 返回的 hash 同视图）。
    /// edit/write/delete 未显式携带 expected_hash 时，用它做自动防漂移校验。
    hash: Option<String>,
    order: u64, // monotonically increasing for recency sort
}

static STATE: OnceLock<Mutex<HashMap<String, FileEntry>>> = OnceLock::new();
static COUNTER: AtomicU64 = AtomicU64::new(0);
const MAX_SUMMARY_FILES: usize = 20;

/// 账本变更记录（跨进程增量同步单元）：serve 端执行工具后产生，
/// 随 HTTP 响应回传，daemon 端 `apply_pending` 回写本地账本，
/// 保证 Environment 的 `<file_state>` 注入不因远程执行而失真。
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct StateEntry {
    pub path: String,
    /// read | edited | created | deleted | moved
    pub op: String,
    pub line_count: usize,
    /// LF canonical 视图 hash（`None` = deleted/moved，无内容指纹）
    pub hash: Option<String>,
}

static PENDING: OnceLock<Mutex<Vec<StateEntry>>> = OnceLock::new();

/// 单次编辑的行号偏移（账本行号修正用）。
///
/// 语义：编辑前 1-based 起始行 `before_line` 及之后的所有行，行号增加
/// `delta`（可负）。read 建立新基线（清空偏移链），write/delete/move 的
/// 全覆盖语义同样使行号失效（清空）。
#[derive(Clone, Debug)]
struct LineShift {
    before_line: usize,
    delta: i64,
}

static SHIFTS: OnceLock<Mutex<HashMap<String, Vec<LineShift>>>> = OnceLock::new();

fn shifts_map() -> &'static Mutex<HashMap<String, Vec<LineShift>>> {
    SHIFTS.get_or_init(|| Mutex::new(HashMap::new()))
}
fn pending() -> &'static Mutex<Vec<StateEntry>> {
    PENDING.get_or_init(|| Mutex::new(Vec::new()))
}

/// 取走自上次以来的全部账本变更（serve 端每次工具执行后调用；清空队列）。
pub fn take_pending() -> Vec<StateEntry> {
    std::mem::take(&mut *pending().lock().unwrap_or_else(|e| e.into_inner()))
}

/// 应用远程账本变更（daemon 端收到 serve 响应后回写本地账本）。
pub fn apply_pending(entries: Vec<StateEntry>) {
    for e in entries {
        let op: &'static str = match e.op.as_str() {
            "read" => "read",
            "edited" => "edited",
            "created" => "created",
            "deleted" => "deleted",
            "moved" => "moved",
            _ => "read",
        };
        if op != "read" {
            crate::file_cache::invalidate(&e.path);
        }
        insert(&e.path, op, e.line_count, e.hash);
    }
}

fn state() -> &'static Mutex<HashMap<String, FileEntry>> {
    STATE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn next_order() -> u64 {
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn insert(path: &str, op: &'static str, line_count: usize, hash: Option<String>) {
    let mut s = state().lock().unwrap_or_else(|e| e.into_inner());
    s.insert(
        path.to_string(),
        FileEntry {
            op,
            line_count,
            hash: hash.clone(),
            order: next_order(),
        },
    );
    // 追加到跨进程同步队列（serve 端经 take_pending 随响应回传）
    pending()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(StateEntry {
            path: path.to_string(),
            op: op.to_string(),
            line_count,
            hash: hash.clone(),
        });
}

/// 任意内容 → LF canonical 视图 hash（与 read 返回的 hash 同视图）。
/// 调用方传入的 content 可能是 CRLF 或混合换行（如 write 落盘内容），
/// 账本必须统一归一化后再哈希，否则 CRLF 文件下账本永远失配。
fn ledger_hash(content: &str) -> String {
    let (lf, _) = crate::file_shared::normalize_newlines(content);
    crate::file_shared::content_hash(&lf)
}

/// Record a file read (full file or range — both establish the ledger).
/// 同时建立行号基线：清空该文件的编辑偏移链。
pub fn record_read(path: &str, content: &str, line_count: usize) {
    crate::file_cache::store(path, content, line_count);
    insert(path, "read", line_count, Some(ledger_hash(content)));
    shifts_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(path);
}

/// Record a file write (create/overwrite/append). 全覆盖语义：行号全失效，
/// 清空偏移链（模型应重新 read 建立基线）。
pub fn record_write(path: &str, content: &str) {
    crate::file_cache::invalidate(path);
    let s = state().lock().unwrap_or_else(|e| e.into_inner());
    let op = if s.contains_key(path) {
        "edited"
    } else {
        "created"
    };
    drop(s);
    insert(
        path,
        op,
        content.lines().count(),
        Some(ledger_hash(content)),
    );
    shifts_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(path);
}

/// Record a file edit with per-hunk line shifts (账本行号修正的数据源)。
/// `shifts` = 编辑前 1-based 起始行 → 行号增量（按序应用，均相对同一快照）。
pub fn record_edit_with_shifts(path: &str, content: &str, shifts: &[(usize, i64)]) {
    crate::file_cache::invalidate(path);
    insert(
        path,
        "edited",
        content.lines().count(),
        Some(ledger_hash(content)),
    );
    let mut m = shifts_map().lock().unwrap_or_else(|e| e.into_inner());
    let list = m.entry(path.to_string()).or_default();
    for (before_line, delta) in shifts {
        if *delta != 0 {
            list.push(LineShift {
                before_line: *before_line,
                delta: *delta,
            });
        }
    }
}

/// 账本行号修正：把"基于最近一次 read 基线"的行号映射到当前行号。
///
/// 返回 `(修正后行号, 总偏移)`；无偏移链（该文件从未被 read 过或链已清）
/// 返回 `Some((line, 0))` 原样，无基线返回 `None`。
pub fn correct_line(path: &str, line: usize) -> Option<(usize, i64)> {
    let m = shifts_map().lock().unwrap_or_else(|e| e.into_inner());
    let list = m.get(path)?;
    if list.is_empty() {
        return Some((line, 0));
    }
    let mut delta: i64 = 0;
    for s in list {
        if line >= s.before_line {
            delta += s.delta;
        }
    }
    Some(((line as i64 + delta).max(1) as usize, delta))
}

/// grep 输出的是实时行号：清空偏移链，防止后续 read 用 grep 行号时被误修正。
pub fn record_grep(path: &str) {
    shifts_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(path);
}

/// Record a file edit.
pub fn record_edit(path: &str, content: &str) {
    crate::file_cache::invalidate(path);
    insert(
        path,
        "edited",
        content.lines().count(),
        Some(ledger_hash(content)),
    );
}

/// Record a file deletion (ledger cleared — the file no longer exists).
pub fn record_delete(path: &str) {
    crate::file_cache::invalidate(path);
    insert(path, "deleted", 0, None);
}

/// 工具侧账本：该路径最近一次被工具看到时的 LF 视图 hash。
/// `None` = 本会话中工具从未见过该文件（尚无校验基线）。
pub fn last_hash(path: &str) -> Option<String> {
    let s = state().lock().unwrap_or_else(|e| e.into_inner());
    s.get(path).and_then(|e| e.hash.clone())
}

/// Generate file state summary. Capped at 20 most recently touched files.
pub fn summary() -> String {
    let s = state().lock().unwrap_or_else(|e| e.into_inner());
    if s.is_empty() {
        return String::new();
    }
    let mut entries: Vec<(&String, &FileEntry)> = s.iter().collect();
    entries.sort_by_key(|(_, e)| -(e.order as i64)); // most recent first
    let total = entries.len();
    entries.truncate(MAX_SUMMARY_FILES);

    let mut out = String::from("<file_state>\n");
    for (path, e) in &entries {
        let lines = if e.line_count > 0 {
            format!("{}L", e.line_count)
        } else {
            String::new()
        };
        out.push_str(&format!(
            "  {:<50} {:>6}  ({})\n",
            path,
            if lines.is_empty() { "—" } else { &lines },
            e.op,
        ));
    }
    if total > MAX_SUMMARY_FILES {
        out.push_str(&format!(
            "  ... ({} more files)\n",
            total - MAX_SUMMARY_FILES
        ));
    }
    out.push_str("</file_state>");
    out
}

/// Clear all tracked state (session reset).
pub fn clear() {
    state().lock().unwrap_or_else(|e| e.into_inner()).clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_roundtrip_applies_remote_ledger() {
        // 模拟 serve 端：record 产生 pending 增量。
        // 注意 pending/state 是进程级全局（lib 测试并行共享），
        // 断言过滤自己的条目；apply_pending 也会产生 pending（daemon 端不回传，
        // 但测试环境里会混入队列），因此取过滤后的最后一条（最新记录）。
        let key = "pending-roundtrip-a.txt";
        // pending 是进程级全局队列且 take_pending 为清空语义：并行测试可能
        // 抢走增量。先吞掉别人的，再写自己的并立刻取走；仍被抢则重试。
        let own = (0..3)
            .find_map(|_| {
                let _ = take_pending();
                record_write(key, "hello\nworld\n");
                let got: Vec<StateEntry> = take_pending()
                    .into_iter()
                    .filter(|e| e.path == key)
                    .collect();
                (!got.is_empty()).then_some(got)
            })
            .expect("own pending entry after retries");
        assert_eq!(own.len(), 1, "one entry for our path");
        assert_eq!(own[0].op, "created");
        assert_eq!(own[0].line_count, 2);
        assert!(own[0].hash.is_some());

        // 模拟 daemon 端：apply 回传增量（不清全局账本，避免踩并行测试）
        apply_pending(own);
        let h = last_hash(key).expect("ledger restored from delta");
        // 与本地 LF canonical 视图 hash 一致
        assert_eq!(h, ledger_hash("hello\nworld\n"));

        // 编辑后 op=edited，hash 更新
        record_edit(key, "hello\nWORLD\n");
        let delta2: Vec<StateEntry> = take_pending()
            .into_iter()
            .filter(|e| e.path == key)
            .collect();
        assert_eq!(delta2.last().map(|e| e.op.as_str()), Some("edited"));
        apply_pending(delta2);
        assert_eq!(last_hash(key).unwrap(), ledger_hash("hello\nWORLD\n"));

        // 删除：hash=None
        record_delete(key);
        let delta3: Vec<StateEntry> = take_pending()
            .into_iter()
            .filter(|e| e.path == key)
            .collect();
        assert_eq!(delta3.last().map(|e| e.op.as_str()), Some("deleted"));
        assert!(delta3.last().unwrap().hash.is_none());
        apply_pending(delta3);
        assert_eq!(last_hash(key), None);
    }

    #[test]
    fn take_pending_clears_the_queue() {
        // PENDING 是进程全局队列：并行测试的 take_pending 会偷走本测试
        // 刚 record 的条目，必须按家规持全仓串行锁。
        let _serial = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // 全局队列：记录唯一条目后断言其出现且取走即清（不依赖队列为空起步）
        let key = "pending-roundtrip-b.txt";
        record_read(key, "x\n", 1);
        let mut saw_own = false;
        let mut queue = take_pending();
        while queue.iter().any(|e| e.path == key) {
            saw_own = true;
            queue = take_pending();
        }
        assert!(saw_own, "own entry observed in the queue");
    }
}
