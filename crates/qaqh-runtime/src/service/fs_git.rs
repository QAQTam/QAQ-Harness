//! service::fs_git — 远端文件浏览 + workspace/git 自由函数。

use serde_json::{Value, json};

use std::io::Read;

/// `fs.list`：目录条目（目录优先 + 名称排序），返回 daemon 侧绝对路径。
///
/// 临时跨端版本有意不做路径沙箱/权限校验，只要求绝对路径。
pub(crate) fn list_remote_directory(path: &str) -> Result<Value, String> {
    let dir = std::path::Path::new(path);
    if !dir.is_absolute() {
        return Err("fs.list requires an absolute path".to_string());
    }
    let mut entries: Vec<Value> = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|e| format!("fs.list {path}: {e}"))? {
        let entry = entry.map_err(|e| format!("fs.list {path}: {e}"))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let entry_path = entry.path();
        let meta = match entry.metadata() {
            Ok(meta) => meta,
            // 软链接/权限问题不阻塞整个目录，标记 unknown 继续。
            Err(_) => {
                entries.push(json!({
                    "name": name,
                    "path": entry_path.to_string_lossy(),
                    "is_dir": false,
                    "is_file": false,
                    "size": 0,
                    "modified_ms": null,
                }));
                continue;
            }
        };
        let modified_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64);
        entries.push(json!({
            "name": name,
            "path": entry_path.to_string_lossy(),
            "is_dir": meta.is_dir(),
            "is_file": meta.is_file(),
            "size": meta.len(),
            "modified_ms": modified_ms,
        }));
    }
    entries.sort_by(|a, b| {
        let (ad, bd) = (
            a["is_dir"].as_bool().unwrap_or(false),
            b["is_dir"].as_bool().unwrap_or(false),
        );
        match bd.cmp(&ad) {
            std::cmp::Ordering::Equal => a["name"]
                .as_str()
                .unwrap_or_default()
                .to_ascii_lowercase()
                .cmp(&b["name"].as_str().unwrap_or_default().to_ascii_lowercase()),
            other => other,
        }
    });
    Ok(Value::Array(entries))
}

/// `fs.read`：文本预览。读满 `max_bytes + 1` 以判断截断；内容按 UTF-8
/// lossy 返回（临时版不处理二进制编码协商）。
pub(crate) fn read_remote_file(path: &str, max_bytes: u64) -> Result<Value, String> {
    let file_path = std::path::Path::new(path);
    if !file_path.is_absolute() {
        return Err("fs.read requires an absolute path".to_string());
    }
    let meta = std::fs::metadata(file_path).map_err(|e| format!("fs.read {path}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("fs.read {path}: not a file"));
    }
    let cap = max_bytes.clamp(1, 8 * 1024 * 1024) as usize;
    let file = std::fs::File::open(file_path).map_err(|e| format!("fs.read {path}: {e}"))?;
    let mut data = Vec::new();
    std::io::Read::take(file, (cap + 1) as u64)
        .read_to_end(&mut data)
        .map_err(|e| format!("fs.read {path}: {e}"))?;
    let truncated = data.len() > cap;
    data.truncate(cap);
    Ok(json!({
        "path": path,
        "size": meta.len(),
        "truncated": truncated,
        "content": String::from_utf8_lossy(&data),
    }))
}

pub(crate) fn workspace(sessions: &qaqh_session::SessionManager, seed: &str) -> String {
    if seed.is_empty() {
        return String::new();
    }
    // 统一数据源：meta.cwd（workspace.txt 退役，读取侧惰性迁移）。
    sessions.workspace_cwd(seed).unwrap_or_default()
}

pub(crate) fn git<F>(
    sessions: &qaqh_session::SessionManager,
    seed: &str,
    operation: F,
    empty: Value,
) -> Result<Value, String>
where
    F: FnOnce(&str) -> Result<String, String>,
{
    let workspace = workspace(sessions, seed);
    if workspace.is_empty() {
        return Ok(empty);
    }
    let value = operation(&workspace)?;
    serde_json::from_str(&value).or_else(|_| Ok(json!(value)))
}

pub(crate) fn qaqh_dir(sessions: &qaqh_session::SessionManager, seed: &str) -> std::path::PathBuf {
    let workspace = workspace(sessions, seed);
    if workspace.is_empty() || workspace == "." {
        qaqh_types::platform::data_dir().join("workspace")
    } else {
        std::path::Path::new(&workspace).join(".qaqh")
    }
}
