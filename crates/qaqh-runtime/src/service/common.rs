//! service::common — handle 与各路由组共用的微型 helper。

use serde_json::Value;

pub(crate) fn err(error: impl std::fmt::Display) -> String {
    error.to_string()
}
pub(crate) fn parse_json_string(value: String) -> Result<Value, String> {
    serde_json::from_str(&value).map_err(err)
}

pub(crate) fn release_freed_heap_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY：malloc_trim 只归还空闲堆内存，不触碰存活分配；glibc 文档
        // 保证线程安全。返回 1 表示确实归还了内存（0 = 无可归还）。
        let returned = unsafe { libc::malloc_trim(0) };
        if returned == 1 {
            log::debug!("[memory] malloc_trim(0): freed heap pages returned to OS");
        }
    }
}

pub(crate) fn command_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("svc-{nanos:x}")
}
