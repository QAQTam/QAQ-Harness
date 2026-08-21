//! File read cache — avoids re-sending unchanged file content to the LLM.
//!
//! Strategy:
//! 1. On file_read: compute content hash → if matches cache AND not consecutive read → return "unchanged"
//! 2. Consecutive reads (same file in adjacent turns): always return full content (model needs re-examination)
//! 3. On file_write/edit/delete: invalidate affected path
//!
//! Cache size capped at 64 entries, LRU eviction via Vec.

use std::sync::Mutex;
use std::sync::OnceLock;

struct CacheEntry {
    path: String,
    hash: String,
    #[allow(dead_code)]
    json: String,
    line_count: usize,
}

static CACHE: OnceLock<Mutex<Vec<CacheEntry>>> = OnceLock::new();
static LAST_READ_PATH: OnceLock<Mutex<Option<String>>> = OnceLock::new();

const MAX_CACHE: usize = 64;

fn cache() -> &'static Mutex<Vec<CacheEntry>> {
    CACHE.get_or_init(|| Mutex::new(Vec::new()))
}

/// Check cache before reading. Returns Some(json) if content unchanged since last read.
/// Consecutive reads (same file twice in a row) always bypass cache — model needs re-examination.
pub fn check(path: &str, content: &str) -> Option<String> {
    let hash = hash_content(content);

    // Check consecutive read
    {
        let mut lr = LAST_READ_PATH
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let is_consecutive = lr.as_ref().is_some_and(|p| p == path);
        *lr = Some(path.to_string());
        if is_consecutive {
            log::info!("[CACHE] file_read consecutive: {} — passing through", path);
            return None;
        }
    }

    // Check cache
    let cache = cache().lock().unwrap_or_else(|e| e.into_inner());
    for e in cache.iter() {
        if e.path == path {
            if e.hash == hash {
                log::info!("[CACHE] file_read hit: {} (hash={})", path, &hash[..8]);
                return Some(
                    serde_json::json!({
                        "timeis": crate::now_utc8(),
                        "status": "ok",
                        "path": path,
                        "hash": crate::file_shared::content_hash(content),
                        "cache_hash": &hash[..8],
                        "total_lines": e.line_count,
                        "unchanged": true,
                        "content": format!("{} unchanged (hash={})", path, &hash[..8]),
                    })
                    .to_string(),
                );
            }
            break;
        }
    }
    None
}

/// Store a successful file_read result for future cache hits.
pub fn store(path: &str, content: &str, line_count: usize) {
    let hash = hash_content(content);
    let hash_short = hash[..8].to_string();
    let mut cache = cache().lock().unwrap_or_else(|e| e.into_inner());
    cache.retain(|e| e.path != path);
    if cache.len() >= MAX_CACHE {
        cache.remove(0);
    }
    cache.push(CacheEntry {
        path: path.to_string(),
        hash,
        json: String::new(),
        line_count,
    });
    log::info!(
        "[CACHE] file_read stored: {} (hash={}, {}L)",
        path,
        hash_short,
        line_count
    );
}

/// Invalidate cache for a path (called on file_write/edit/delete/move).
pub fn invalidate(path: &str) {
    let mut cache = cache().lock().unwrap_or_else(|e| e.into_inner());
    let before = cache.len();
    cache.retain(|e| e.path != path);
    if cache.len() < before {
        log::info!("[CACHE] invalidated: {}", path);
    }
}

fn hash_content(s: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Clear all cache (session reset).
pub fn clear() {
    cache().lock().unwrap_or_else(|e| e.into_inner()).clear();
    if let Ok(mut lr) = LAST_READ_PATH.get_or_init(|| Mutex::new(None)).lock() {
        *lr = None;
    }
}
