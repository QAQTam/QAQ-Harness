//! 测试用 JSON-LP 行传输：把一行 JSON 反序列化为 agent 命令消息。
//!
//! 生产路径是 `Loop::from_channels` 的 typed channel，不经过行解析；
//! 本模块仅服务于测试 harness 模拟旧管道输入（`Loop::new_ipc` 已退役）。

use std::io::BufRead;

use qaqh_ringing::RingingWorkerCommandEnvelope;

/// 读取一行并反序列化为命令消息。空行返回 `Ok(None)`。
pub fn read_worker_command_frame<R: BufRead>(
    r: &mut R,
) -> std::io::Result<Option<RingingWorkerCommandEnvelope>> {
    let mut line = String::new();
    let n = r.read_line(&mut line)?;
    if n == 0 || line.trim().is_empty() {
        return Ok(None);
    }
    serde_json::from_str(line.trim())
        .map(Some)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_ringing::RingingCommand as RC;

    #[test]
    fn typed_command_line_is_parsed() {
        let env = RingingWorkerCommandEnvelope::new(
            "s1",
            "cmd-1",
            RC::Conversation(qaqh_domain::ConversationCommand::ConversationCancel {
                turn_id: None,
            }),
        );
        let json = serde_json::to_string(&env).expect("serialize");
        let mut reader = std::io::Cursor::new(format!("{json}\n"));
        let frame = read_worker_command_frame(&mut reader)
            .expect("read")
            .expect("frame");
        assert_eq!(frame.command_id, "cmd-1");
        assert_eq!(frame.seed, "s1");
    }

    #[test]
    fn blank_line_returns_none() {
        let mut reader = std::io::Cursor::new("\n\n");
        assert!(
            read_worker_command_frame(&mut reader)
                .expect("read")
                .is_none()
        );
    }

    #[test]
    fn malformed_line_is_invalid_data() {
        let mut reader = std::io::Cursor::new("not-json\n");
        let err = read_worker_command_frame(&mut reader).expect_err("must reject");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
