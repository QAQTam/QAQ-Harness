//! Pending command store — Ringing V1 command idempotency + 4096 cap.
//!
//! Extracted from `qaqh-daemon/src/ringing_http.rs:2866` for sharing between
//! legacy TCP and axum paths.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use qaqh_ringing::{RingingCommandState, RingingCommandStatus, RingingEvent, RingingEventEnvelope};

/// 已 accepted 命令的幂等表（有界 TTL；accepted 后断线重试不得重复执行）。
#[derive(Debug, Default)]
pub struct PendingCommandStore {
    accepted: HashMap<String, CommandReceipt>,
    max_entries: usize,
    persistence_path: Option<PathBuf>,
}

/// 冻结告警条目：Accepted/Running 超时无终态的 receipt 快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleRunningReceipt {
    pub command_id: String,
    pub state: RingingCommandState,
    pub age_secs: u64,
    pub client_session_id: Option<String>,
}

#[derive(Debug, Clone)]
struct CommandReceipt {
    fingerprint: String,
    client_session_id: Option<String>,
    accepted_at: Instant,
    state: RingingCommandState,
    terminal_event_id: Option<String>,
    error_code: Option<String>,
    /// 上次冻结告警时刻（epoch ms，仅内存不持久化）：同一 receipt 的重复
    /// 告警按间隔限频，避免周期巡检刷屏。
    last_stale_warn_ms: Option<u64>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PersistedCommandReceipt {
    fingerprint: String,
    #[serde(default)]
    client_session_id: Option<String>,
    accepted_at_ms: u64,
    state: RingingCommandState,
    #[serde(default)]
    terminal_event_id: Option<String>,
    #[serde(default)]
    error_code: Option<String>,
}

impl PendingCommandStore {
    pub fn new() -> Self {
        Self {
            accepted: HashMap::new(),
            max_entries: 4096,
            persistence_path: None,
        }
    }

    /// Receipt 存在 daemon 数据目录的独立 Ringing V1 namespace；只保存哈希，
    /// 不把命令正文、用户文本或附件元数据写入磁盘。
    pub fn new_persistent() -> Self {
        let data_dir = qaqh_types::platform::data_dir();
        let path = data_dir.join("ringing-command-receipts.json");
        let mut store = Self {
            persistence_path: Some(path.clone()),
            ..Self::new()
        };
        store.load(&path);
        store
    }

    fn load(&mut self, path: &std::path::Path) {
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        let Ok(saved) = serde_json::from_slice::<HashMap<String, PersistedCommandReceipt>>(&bytes)
        else {
            log::warn!("[ringing] command receipt store is unreadable; starting empty");
            return;
        };
        let now_ms = unix_millis();
        for (command_id, receipt) in saved {
            let Some(age) = now_ms.checked_sub(receipt.accepted_at_ms) else {
                continue;
            };
            if age >= RECEIPT_TTL.as_millis() as u64 {
                continue;
            }
            self.accepted.insert(
                command_id,
                CommandReceipt {
                    fingerprint: receipt.fingerprint,
                    client_session_id: receipt.client_session_id,
                    accepted_at: Instant::now() - Duration::from_millis(age),
                    state: receipt.state,
                    terminal_event_id: receipt.terminal_event_id,
                    error_code: receipt.error_code,
                    last_stale_warn_ms: None,
                },
            );
        }
    }

    fn persist(&self) {
        let Some(path) = &self.persistence_path else {
            return;
        };
        let saved: HashMap<_, _> = self
            .accepted
            .iter()
            .filter_map(|(command_id, receipt)| {
                let age = receipt.accepted_at.elapsed();
                (age < RECEIPT_TTL).then(|| {
                    (
                        command_id.clone(),
                        PersistedCommandReceipt {
                            fingerprint: receipt.fingerprint.clone(),
                            client_session_id: receipt.client_session_id.clone(),
                            accepted_at_ms: unix_millis().saturating_sub(age.as_millis() as u64),
                            state: receipt.state,
                            terminal_event_id: receipt.terminal_event_id.clone(),
                            error_code: receipt.error_code.clone(),
                        },
                    )
                })
            })
            .collect();
        let Ok(bytes) = serde_json::to_vec(&saved) else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// 记录 accepted。返回 false 表示重复（已 accepted 且未过期）。
    pub fn record(&mut self, command_id: &str) -> bool {
        self.record_fingerprint(command_id, command_id)
            .unwrap_or(false)
    }

    /// 预留 receipt；相同 ID 不同 payload 是协议错误。
    pub fn record_fingerprint(&mut self, command_id: &str, fingerprint: &str) -> Result<bool, ()> {
        self.record_fingerprint_owned(command_id, fingerprint, None)
    }

    pub fn record_fingerprint_for_session(
        &mut self,
        command_id: &str,
        fingerprint: &str,
        client_session_id: &str,
    ) -> Result<bool, ()> {
        self.record_fingerprint_owned(command_id, fingerprint, Some(client_session_id))
    }

    fn record_fingerprint_owned(
        &mut self,
        command_id: &str,
        fingerprint: &str,
        client_session_id: Option<&str>,
    ) -> Result<bool, ()> {
        let now = Instant::now();
        if let Some(receipt) = self.accepted.get(command_id) {
            if receipt.accepted_at + RECEIPT_TTL > now {
                if receipt.fingerprint != fingerprint
                    || receipt.client_session_id.as_deref() != client_session_id
                {
                    return Err(());
                }
                return Ok(false); // 重复：已接受且在 TTL 内
            }
        }
        self.accepted.insert(
            command_id.to_string(),
            CommandReceipt {
                fingerprint: fingerprint.to_string(),
                client_session_id: client_session_id.map(str::to_string),
                accepted_at: now,
                state: RingingCommandState::Accepted,
                terminal_event_id: None,
                error_code: None,
                last_stale_warn_ms: None,
            },
        );
        while self.accepted.len() > self.max_entries {
            let victim = self
                .accepted
                .iter()
                .min_by_key(|(_, receipt)| receipt.accepted_at)
                .map(|(id, _)| id.clone())
                .expect("non-empty");
            self.accepted.remove(&victim);
        }
        self.persist();
        Ok(true)
    }

    pub fn is_known(&self, command_id: &str) -> bool {
        self.accepted
            .get(command_id)
            .is_some_and(|receipt| receipt.accepted_at + RECEIPT_TTL > Instant::now())
    }

    /// 转发失败回滚预留。
    pub fn rollback(&mut self, command_id: &str) {
        self.accepted.remove(command_id);
        self.persist();
    }

    pub fn mark_running(&mut self, command_id: &str) {
        if let Some(receipt) = self.accepted.get_mut(command_id) {
            if receipt.state == RingingCommandState::Accepted {
                receipt.state = RingingCommandState::Running;
            }
            self.persist();
        }
    }

    /// 将带 causation_id 的可靠业务终态折叠进命令 receipt。ACK 只表示
    /// accepted；这里为断线后的 command-status 查询提供最终结果。
    pub fn observe_terminal_event(&mut self, envelope: &RingingEventEnvelope) {
        let Some(command_id) = envelope.causation_id.as_deref() else {
            return;
        };
        let terminal = match &envelope.event {
            RingingEvent::Control(qaqh_domain::ControlEvent::OperationFailed { error, .. }) => {
                Some((RingingCommandState::Failed, Some(error.code.clone())))
            }
            RingingEvent::Control(
                qaqh_domain::ControlEvent::InteractionResolved { .. }
                | qaqh_domain::ControlEvent::PlanReviewResolved { .. }
                | qaqh_domain::ControlEvent::SkillsUpdated { .. }
                | qaqh_domain::ControlEvent::SessionStateChanged { .. }
                | qaqh_domain::ControlEvent::OperationCompleted { .. },
            ) => Some((RingingCommandState::Succeeded, None)),
            RingingEvent::Conversation(qaqh_domain::ConversationEvent::TurnFailed {
                error,
                ..
            }) => Some((RingingCommandState::Failed, Some(error.code.clone()))),
            RingingEvent::Conversation(
                qaqh_domain::ConversationEvent::TurnCompleted { .. }
                | qaqh_domain::ConversationEvent::ConversationCancelled { .. },
            ) => Some((RingingCommandState::Succeeded, None)),
            RingingEvent::Conversation(qaqh_domain::ConversationEvent::CompactFinished {
                status,
                ..
            }) => match status {
                qaqh_domain::CompactStatus::Failed => {
                    Some((RingingCommandState::Failed, Some("compact_failed".into())))
                }
                _ => Some((RingingCommandState::Succeeded, None)),
            },
            RingingEvent::Tool(qaqh_domain::ToolEvent::ToolFinished { result, .. }) => {
                if result.status.is_failure() {
                    Some((
                        RingingCommandState::Failed,
                        result.error.as_ref().map(|error| error.code.clone()),
                    ))
                } else {
                    Some((RingingCommandState::Succeeded, None))
                }
            }
            _ => None,
        };
        if let Some((state, error_code)) = terminal {
            self.mark_terminal(
                command_id,
                state,
                Some(envelope.event_id.clone()),
                error_code,
            );
        }
    }

    pub fn mark_terminal(
        &mut self,
        command_id: &str,
        state: RingingCommandState,
        event_id: Option<String>,
        error_code: Option<String>,
    ) {
        if let Some(receipt) = self.accepted.get_mut(command_id) {
            receipt.state = state;
            receipt.terminal_event_id = event_id;
            receipt.error_code = error_code;
            self.persist();
        }
    }

    /// 巡检 Accepted/Running 超时无终态的 receipt 并告警（返回告警清单）。
    ///
    /// 冻结事故（2026-09-02，session 692d1605 t7）："ConversationCancel accepted
    /// 但永无终态"的僵尸只能靠人肉轮询发现。此方法由 daemon 周期任务调用：
    /// 超过 `warn_after` 未达终态即 WARN，同一 receipt 按 `repeat_interval`
    /// 限频重复告警，直至终态折叠。纯内存限频状态，不触发 persist。
    pub fn warn_stale_running(
        &mut self,
        warn_after: Duration,
        repeat_interval: Duration,
    ) -> Vec<StaleRunningReceipt> {
        let now = Instant::now();
        let now_ms = unix_millis();
        let repeat_ms = repeat_interval.as_millis() as u64;
        let mut stale = Vec::new();
        for (command_id, receipt) in self.accepted.iter_mut() {
            if !matches!(
                receipt.state,
                RingingCommandState::Accepted | RingingCommandState::Running
            ) {
                continue;
            }
            let age = now.saturating_duration_since(receipt.accepted_at);
            if age < warn_after {
                continue;
            }
            let warned_recently = receipt
                .last_stale_warn_ms
                .is_some_and(|last| now_ms.saturating_sub(last) < repeat_ms);
            if warned_recently {
                continue;
            }
            receipt.last_stale_warn_ms = Some(now_ms);
            stale.push(StaleRunningReceipt {
                command_id: command_id.clone(),
                state: receipt.state,
                age_secs: age.as_secs(),
                client_session_id: receipt.client_session_id.clone(),
            });
        }
        for entry in &stale {
            log::warn!(
                "[ringing] command receipt stuck in {:?} for {}s (no terminal event): {} client_session={:?} — possible frozen worker/seal path",
                entry.state,
                entry.age_secs,
                entry.command_id,
                entry.client_session_id
            );
        }
        stale
    }

    pub fn status_for_session(
        &self,
        command_id: &str,
        client_session_id: &str,
    ) -> Option<RingingCommandStatus> {
        self.accepted.get(command_id).and_then(|receipt| {
            (receipt.accepted_at + RECEIPT_TTL > Instant::now()
                && receipt.client_session_id.as_deref() == Some(client_session_id))
            .then(|| RingingCommandStatus {
                command_id: command_id.to_string(),
                state: receipt.state,
                payload_fingerprint: receipt.fingerprint.clone(),
                terminal_event_id: receipt.terminal_event_id.clone(),
                error_code: receipt.error_code.clone(),
            })
        })
    }
}

const RECEIPT_TTL: Duration = Duration::from_secs(300);

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_domain::{ToolEvent, ToolResult};
    use qaqh_ringing::{RingingEvent, RingingEventEnvelope};

    #[test]
    fn pending_command_idempotency() {
        let mut store = PendingCommandStore::new();
        assert!(store.record("cmd-1"), "first accept");
        assert!(!store.record("cmd-1"), "duplicate within TTL rejected");
        assert!(store.is_known("cmd-1"));
        assert!(store.record("cmd-2"), "distinct id accepted");
        store.rollback("cmd-2");
        assert!(store.record("cmd-2"), "retry after rollback accepted");
    }

    #[test]
    fn command_receipts_are_scoped_to_the_owning_client_session() {
        let mut store = PendingCommandStore::new();
        assert!(
            store
                .record_fingerprint_for_session("cmd-owner", "fp", "session-a")
                .expect("first accept")
        );
        assert!(store.status_for_session("cmd-owner", "session-a").is_some());
        assert!(store.status_for_session("cmd-owner", "session-b").is_none());
        assert!(
            store
                .record_fingerprint_for_session("cmd-owner", "fp", "session-b")
                .is_err()
        );
    }

    #[test]
    fn causally_linked_terminal_event_completes_receipt_without_running_downgrade() {
        let mut store = PendingCommandStore::new();
        assert!(
            store
                .record_fingerprint_for_session("cmd-1", "fp", "session-a")
                .expect("accept")
        );
        let envelope = RingingEventEnvelope::new(
            "seed",
            1,
            1,
            1,
            "event-1",
            RingingEvent::Tool(ToolEvent::ToolFinished {
                tool_call_id: "call".into(),
                turn_id: "turn".into(),
                round_num: 0,
                result: ToolResult::ok("ok"),
            }),
        )
        .with_causation("cmd-1");
        store.observe_terminal_event(&envelope);
        store.mark_running("cmd-1");
        let status = store
            .status_for_session("cmd-1", "session-a")
            .expect("status");
        assert_eq!(status.state, RingingCommandState::Succeeded);
        assert_eq!(status.terminal_event_id.as_deref(), Some("event-1"));
    }
}

#[cfg(test)]
mod stale_running_tests {
    use super::*;

    /// 冻结事故（2026-09-02）回归：Accepted/Running 超时无终态必须被巡检
    /// 告警；终态折叠后不再告警；限频间隔内不重复告警。
    #[test]
    fn stale_running_receipts_are_warned_and_rate_limited() {
        let mut store = PendingCommandStore::new();
        assert!(store.record("cmd-stuck"));
        // 0s 阈值：任何 Accepted/Running 立即命中。
        let first = store.warn_stale_running(Duration::ZERO, Duration::from_secs(60));
        assert_eq!(first.len(), 1, "stale receipt must be reported once");
        assert_eq!(first[0].command_id, "cmd-stuck");
        assert_eq!(first[0].state, RingingCommandState::Accepted);
        // 限频：重复间隔内再次巡检不再报告。
        let second = store.warn_stale_running(Duration::ZERO, Duration::from_secs(60));
        assert!(
            second.is_empty(),
            "repeat within interval must be suppressed"
        );
        // 限频归零（间隔 0）：再次报告。
        let third = store.warn_stale_running(Duration::ZERO, Duration::ZERO);
        assert_eq!(third.len(), 1, "repeat_interval=0 re-warns");
        // 终态折叠后不再告警。
        store.mark_terminal("cmd-stuck", RingingCommandState::Succeeded, None, None);
        let fourth = store.warn_stale_running(Duration::ZERO, Duration::ZERO);
        assert!(fourth.is_empty(), "terminal receipts are never stale");
    }
}
