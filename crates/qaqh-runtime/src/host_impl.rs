//! `SubagentHost` 的进程内实现（Knife-1 step-2 收尾）。
//!
//! 主 session 与子代理 loop 均作为 daemon 线程内 in-process actor 运行
//! （PR #17/#19/#21），`spawn_subagent` 工具 handler 也在 daemon 进程内
//! 执行——此前它经 daemon HTTP/SSE 回连自己，属于多余的进程内回环。
//!
//! 本模块让 `QaqhService` 直接实现 [`qaqh_subagent::SubagentHost`]：
//! 工具 handler 通过该宿主句柄直达进程内 `AgentRegistry` + `RingingHub`，
//! 不再建立 HTTP/SSE 连接。事件订阅直接走 hub 进程内 broadcast。

use std::sync::mpsc;
use std::time::Duration;

use qaqh_domain::RingingChannel;
use qaqh_ringing::{RingingEventEnvelope, RingingWorkerCommandEnvelope};
use qaqh_subagent::{ContentRef, EventBatch, SubagentHost};

use super::QaqhService;

impl SubagentHost for QaqhService {
    fn spawn_subagent(
        &self,
        tools: &[String],
        model: Option<&str>,
        base_url: Option<&str>,
        max_tokens: Option<u32>,
        workspace: Option<&str>,
    ) -> Result<String, String> {
        let seed = qaqh_session::SessionManager::generate_seed();
        if let Some(workspace) = workspace.filter(|w| !w.is_empty() && *w != ".") {
            // 子代理继承主代理工作区（写入 meta.cwd，与 daemon `subagent.spawn` action 一致）。
            qaqh_session::SessionManager::global().set_cwd(&seed, workspace, false);
            log::info!("[SUBAGENT-HOST] inherited workspace for seed={seed}: {workspace}");
        }
        self.registry()?
            .spawn_subagent(&seed, tools, model, base_url, max_tokens)?;
        log::info!(
            "[SUBAGENT-HOST] spawned subagent seed={seed} tools={}",
            tools.len()
        );
        Ok(seed)
    }

    fn send_ringing(
        &self,
        seed: &str,
        command: qaqh_ringing::RingingCommand,
    ) -> Result<(), String> {
        let id = format!("host-{:x}", nanos());
        let env = RingingWorkerCommandEnvelope::new(seed, id, command);
        self.registry()?.send_ringing(seed, &env)
    }

    fn subscribe(&self, seed: &str) -> mpsc::Receiver<EventBatch> {
        let (tx, rx) = mpsc::channel::<EventBatch>();
        let hub = match self.hub.get() {
            Some(hub) => hub.clone(),
            None => {
                log::error!("[SUBAGENT-HOST] subscribe {seed}: Ringing hub not attached");
                return rx;
            }
        };
        let epoch = hub.epoch().to_string();
        let seed_own = seed.to_string();
        for channel in [
            RingingChannel::Control,
            RingingChannel::Conversation,
            RingingChannel::Tool,
        ] {
            let mut hub_rx = hub.subscribe(channel);
            let tx = tx.clone();
            let seed = seed_own.clone();
            let epoch = epoch.clone();
            std::thread::Builder::new()
                .name(format!("qaqh-subagent-sub-{seed_own}"))
                .spawn(move || {
                    // broadcast::Receiver 非 Send… 但 tokio broadcast Receiver 是 Send。
                    // 用 try_recv 轮询（无 block_on 依赖），聚合到 std mpsc。
                    loop {
                        match hub_rx.try_recv() {
                            Ok(env) => {
                                if env.seed != seed {
                                    continue;
                                }
                                let batch = envelope_to_batch(channel, env, &epoch);
                                if tx.send(batch).is_err() {
                                    break;
                                }
                            }
                            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                                std::thread::sleep(Duration::from_millis(20));
                            }
                            Err(_) => break, // Closed / Lagged：终止桥接
                        }
                    }
                })
                .ok();
        }
        rx
    }

    fn download_content(&self, seed: &str, reference: &ContentRef) -> Result<Vec<u8>, String> {
        let hub = self
            .hub
            .get()
            .ok_or_else(|| "Ringing hub not attached".to_string())?;
        let entry = hub
            .get_content(seed, &reference.content_id)
            .ok_or_else(|| format!("content {} not found", reference.content_id))?;
        let digest = {
            use sha2::Digest;
            let hash = sha2::Sha256::digest(&entry.bytes);
            hash.iter().map(|b| format!("{b:02x}")).collect::<String>()
        };
        if !digest.eq_ignore_ascii_case(&reference.sha256) {
            return Err(format!(
                "content digest mismatch for {}: expected {}, received {digest}",
                reference.content_id, reference.sha256
            ));
        }
        Ok(entry.bytes)
    }

    fn close(&self, seed: &str) -> Result<(), String> {
        // 与 daemon action `session.close`/`SessionClose` 拦截一致：关闭 registry
        // 实例并清理临时会话；结果已注入主会话 + 终态已回写注册表，残留不丢数据。
        self.close_session(seed, None)
    }
}

/// 把单条 hub 事件信封包装为规范 EventBatch（与 client 的 `envelope_to_batch` 同构）。
fn envelope_to_batch(
    channel: RingingChannel,
    env: RingingEventEnvelope,
    server_epoch: &str,
) -> EventBatch {
    let seq = env.stream_seq;
    EventBatch {
        schema: qaqh_ringing::protocol::RINGING_SCHEMA.to_string(),
        version: qaqh_ringing::protocol::RINGING_VERSION,
        channel,
        seed: env.seed.clone(),
        server_epoch: server_epoch.to_string(),
        from_stream_seq: seq,
        to_stream_seq: seq,
        envelopes: vec![env],
    }
}

fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}
