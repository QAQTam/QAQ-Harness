//! Content-addressed image byte store（A-2 L0：图片磁盘外置）。
//!
//! 磁盘布局：`{data_dir}/images/{sha256}.{ext}`，存 **base64 文本**而非解码字节：
//! - 写侧免 decode（上传 / 工具结果拿到的本就是 base64 形态）；
//! - 读侧（gate 请求降格 / read_image registry peek）需要的正是 base64 文本，
//!   免 re-encode；
//! - `bytes_len` 直接等于 base64 文本长度，与既有 `[Image #N]` 占位符的
//!   `~{data.len()}` 显示语义一致。
//!
//! 内容寻址（sha256 对 base64 文本求哈希）天然去重：同一图片多轮 / 多块引用
//! 只落盘一份。写入用 temp+rename 原子替换；同 sha 已存在则直接复用（幂等）。

use std::fs;
use std::path::PathBuf;

use crate::platform::data_dir;

/// 图片存储根目录（与 SessionManager 同一 data root）。
pub fn images_dir() -> PathBuf {
    data_dir().join("images")
}

/// 存储 base64 图片文本，返回内容寻址 id（sha256 hex）。幂等。
pub fn store_image_b64(b64: &str, mime_type: &str) -> Result<String, String> {
    if b64.is_empty() {
        return Err("image payload is empty".to_string());
    }
    let sha = sha256_hex(b64.as_bytes());
    let dir = images_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("image store mkdir: {e}"))?;
    let path = dir.join(format!("{sha}.{}", ext_for_mime(mime_type)));
    if !path.exists() {
        let tmp = dir.join(format!(".{sha}.tmp"));
        fs::write(&tmp, b64).map_err(|e| format!("image store write: {e}"))?;
        fs::rename(&tmp, &path).map_err(|e| format!("image store rename: {e}"))?;
    }
    Ok(sha)
}

/// 按内容寻址 id 读回 base64 文本。
pub fn load_image_b64(sha256: &str, mime_type: &str) -> Result<String, String> {
    let path = images_dir().join(format!("{sha256}.{}", ext_for_mime(mime_type)));
    fs::read_to_string(&path).map_err(|e| format!("image {sha256} not loadable: {e}"))
}

/// mime → 磁盘扩展名（仅用于文件命名，读取时由调用方携带同一 mime 推导）。
pub fn ext_for_mime(mime_type: &str) -> &'static str {
    match mime_type {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        // png 为缺省：历史行为里 upload 侧未严格校验 mime，宁可用最常见扩展名。
        _ => "png",
    }
}

/// sha256 hex（与 runtime content_store 的实现同形；types 层自持一份，
/// 避免 types → runtime 反向依赖）。
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_and_load_round_trip_is_idempotent() {
        let dir = images_dir();
        let _ = fs::remove_dir_all(&dir);
        let b64 = "aGVsbG8gcXNxag=="; // "hello qsqj"
        let sha = store_image_b64(b64, "image/png").expect("store");
        assert_eq!(sha.len(), 64);
        // 幂等：重复存储同内容，返回同 sha、不产生新文件。
        let sha2 = store_image_b64(b64, "image/png").expect("re-store");
        assert_eq!(sha, sha2);
        assert_eq!(load_image_b64(&sha, "image/png").expect("load"), b64);
        let files: Vec<_> = fs::read_dir(&dir).unwrap().collect();
        assert_eq!(files.len(), 1, "content addressing must dedup");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_image_is_actionable_error() {
        let err = load_image_b64("deadbeef", "image/png").unwrap_err();
        assert!(err.contains("deadbeef"));
    }

    #[test]
    fn ext_for_mime_covers_common_types() {
        assert_eq!(ext_for_mime("image/jpeg"), "jpg");
        assert_eq!(ext_for_mime("image/webp"), "webp");
        assert_eq!(ext_for_mime("image/unknown"), "png");
    }

    #[test]
    fn image_ref_block_serializes_with_tagged_shape() {
        let block = crate::message::ContentBlock::image_ref("abc123", "image/png", 42);
        let json = serde_json::to_value(&block).expect("serialize");
        assert_eq!(json["type"], "image_ref");
        assert_eq!(json["sha256"], "abc123");
        assert_eq!(json["mime_type"], "image/png");
        assert_eq!(json["bytes_len"], 42);
        let back: crate::message::ContentBlock = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, block);
    }
}
