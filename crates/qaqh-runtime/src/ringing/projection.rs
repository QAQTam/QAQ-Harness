//! 领域 snapshot projection。
//!
//! PLAN 硬规则：**Snapshot 必须表达领域状态，禁止用事件数组模拟状态**；
//! snapshot 只能从领域状态/领域事件生成，不从 legacy wire 反推。
//!
//! 本模块维护每 seed+channel 的领域状态（JSON 视图），由 `apply` 事件驱动。
//! 完整强类型投影在频道迁移（T9/T10）时按需扩展；基础骨架在此。

use std::collections::HashMap;

use qaqh_domain::{DomainEvent, RingingChannel};
use qaqh_ringing::RingingChannelSnapshot;

/// 频道快照投影。
#[derive(Debug, Default)]
pub struct SnapshotProjector {
    state: HashMap<(RingingChannel, String), serde_json::Value>,
    revisions: HashMap<(RingingChannel, String), u64>,
}

impl SnapshotProjector {
    pub fn new() -> Self {
        Self::default()
    }

    /// 应用领域事件并更新该 seed+channel 的领域状态。
    /// 返回是否发生状态变更（用于决定是否 bump state_revision）。
    pub fn apply(&mut self, channel: RingingChannel, seed: &str, event: &DomainEvent) -> bool {
        let key = (channel, seed.to_string());
        let entry = self.state.entry(key.clone()).or_insert_with(|| {
            serde_json::json!({
                "seed": seed,
                "channel": channel.as_str(),
                "revision": 0,
            })
        });
        let changed = Self::fold(channel, entry, event);
        if changed {
            let rev = self.revisions.entry(key).or_default();
            *rev = rev.saturating_add(1);
            entry["revision"] = serde_json::Value::from(*rev);
        }
        changed
    }

    /// 生成领域快照（wire 层 `RingingChannelSnapshot`）。
    pub fn snapshot_for(
        &self,
        channel: RingingChannel,
        seed: &str,
        baseline_stream_seq: u64,
    ) -> RingingChannelSnapshot {
        let state = self
            .state
            .get(&(channel, seed.to_string()))
            .cloned()
            .unwrap_or_else(|| {
                serde_json::json!({
                    "seed": seed,
                    "channel": channel.as_str(),
                    "revision": 0,
                })
            });
        let revision = self
            .revisions
            .get(&(channel, seed.to_string()))
            .copied()
            .unwrap_or(0);
        RingingChannelSnapshot::new(channel, seed, baseline_stream_seq, revision, state)
    }

    pub fn revision(&self, channel: RingingChannel, seed: &str) -> u64 {
        self.revisions
            .get(&(channel, seed.to_string()))
            .copied()
            .unwrap_or(0)
    }

    /// 领域状态折叠（按频道）。仅实现基础状态演化；
    /// 强类型字段（turns/tools/control）在 T9/T10 频道迁移时补齐。
    fn fold(channel: RingingChannel, state: &mut serde_json::Value, event: &DomainEvent) -> bool {
        match (channel, event) {
            (RingingChannel::Control, DomainEvent::Control(ce)) => {
                use qaqh_domain::ControlEvent as CE;
                match ce {
                    CE::SessionStateChanged { state: s, .. } => {
                        state["session_state"] = serde_json::json!(s);
                        true
                    }
                    CE::SessionActivityChanged { state: s, .. } => {
                        state["activity"] = serde_json::json!(s);
                        true
                    }
                    CE::SessionMetaChanged { title, .. } => {
                        // 元数据变更（标题）：快照只记录"已变更"事实，
                        // 具体值由前端 session.list 重拉（全量权威）。
                        state["meta_changed"] = serde_json::json!(title);
                        true
                    }
                    CE::AgentLifecycleChanged { state: s } => {
                        state["agent_lifecycle"] = serde_json::json!(s);
                        true
                    }
                    CE::InteractionRequested { interaction_id, .. } => {
                        state["pending_interaction"] =
                            serde_json::json!({ "id": interaction_id, "kind": "ask" });
                        true
                    }
                    CE::InteractionResolved { .. } => {
                        state["pending_interaction"] = serde_json::Value::Null;
                        true
                    }
                    CE::PlanReviewRequested { interaction_id, .. } => {
                        state["pending_interaction"] =
                            serde_json::json!({ "id": interaction_id, "kind": "plan" });
                        true
                    }
                    CE::PlanReviewResolved { .. } => {
                        state["pending_interaction"] = serde_json::Value::Null;
                        true
                    }
                    CE::OperationFailed { .. } => {
                        state["last_failure"] = serde_json::json!({ "occurred": true });
                        true
                    }
                    CE::OperationCompleted { .. } => false,
                    CE::SystemNotice { notice_id, .. } => {
                        state["last_notice"] = serde_json::json!(notice_id);
                        true
                    }
                    CE::DashboardSnapshot { snapshot } => {
                        state["dashboard_snapshot"] = serde_json::json!(snapshot);
                        true
                    }
                    CE::SkillsUpdated { .. }
                    | CE::DashboardUpdated { .. }
                    // 瞬态终态推送：不折叠进快照（tracker 收敛走实时事件）。
                    | CE::SubagentStatus { .. } => false,
                }
            }
            (RingingChannel::Conversation, DomainEvent::Conversation(ce)) => {
                use qaqh_domain::ConversationEvent as CE;
                match ce {
                    CE::TurnStarted { turn_id, .. } => {
                        state["active_turn"] = serde_json::json!(turn_id);
                        true
                    }
                    CE::TurnCompleted { turn_id, .. } => {
                        state["last_completed_turn"] = serde_json::json!(turn_id);
                        state["active_turn"] = serde_json::Value::Null;
                        true
                    }
                    CE::TurnFailed { turn_id, .. } => {
                        state["last_failed_turn"] = serde_json::json!(turn_id);
                        state["active_turn"] = serde_json::Value::Null;
                        true
                    }
                    CE::RoundCompleted {
                        turn_id,
                        round_num,
                        is_final,
                        ..
                    } => {
                        state["last_round"] = serde_json::json!({
                            "turn_id": turn_id,
                            "round_num": round_num,
                            "final": is_final,
                        });
                        true
                    }
                    CE::CompactStarted { compact_id, .. } => {
                        state["compact_status"] = serde_json::json!("running");
                        state["compact_id"] = serde_json::json!(compact_id);
                        true
                    }
                    CE::CompactFinished {
                        compact_id, status, ..
                    } => {
                        state["compact_status"] = serde_json::json!(status);
                        state["compact_id"] = serde_json::json!(compact_id);
                        true
                    }
                    CE::ConversationCancelled { .. } => {
                        state["active_turn"] = serde_json::Value::Null;
                        state["cancelled"] = serde_json::Value::Bool(true);
                        true
                    }
                    _ => false, // delta/usage/progress 不进快照
                }
            }
            (RingingChannel::Tool, DomainEvent::Tool(te)) => {
                use qaqh_domain::ToolEvent as TE;
                match te {
                    TE::ToolPermissionRequested { tool_call_id, .. } => {
                        state["pending_permission"] = serde_json::json!(tool_call_id);
                        true
                    }
                    TE::ToolFinished { tool_call_id, .. } => {
                        state["last_finished"] = serde_json::json!(tool_call_id);
                        state["pending_permission"] = serde_json::Value::Null;
                        // ToolStarted 写入的 running 必须在终态清除，否则 daemon
                        // 重启重放 journal 后 tool 快照永远携带陈旧 running 列表
                        // （异常中断时 ToolFinished 从未到达，孤儿 turn 的收尾
                        // 也会依赖本分支清空）。
                        state["running"] = serde_json::Value::Null;
                        true
                    }
                    TE::ToolStarted {
                        tool_call_id,
                        turn_id,
                        round_num,
                        ..
                    } => {
                        // 对象化：孤儿收尾（seal_orphan_channel_state）需要
                        // turn_id/round_num 才能发布完整 ToolFinished 终态。
                        state["running"] = serde_json::json!([{
                            "tool_call_id": tool_call_id,
                            "turn_id": turn_id,
                            "round_num": round_num,
                        }]);
                        true
                    }
                    _ => false, // progress/prepared/notice/audit/code 不进快照
                }
            }
            // 频道与事件不匹配：拒绝投影（不变量：事件必须进入正确频道）
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_domain::{
        ActivityState, AgentLifecycleState, ControlEvent, ConversationEvent, SessionState,
        ToolEvent,
    };

    #[test]
    fn control_snapshot_tracks_interaction_pending() {
        let mut p = SnapshotProjector::new();
        let ev = |ce: ControlEvent| DomainEvent::Control(ce);
        p.apply(
            RingingChannel::Control,
            "s",
            &ev(ControlEvent::SessionStateChanged {
                seed: "s".into(),
                state: SessionState::Created,
            }),
        );
        assert!(p.apply(
            RingingChannel::Control,
            "s",
            &ev(ControlEvent::InteractionRequested {
                interaction_id: "i1".into(),
                turn_id: "t1".into(),
                mode: qaqh_domain::AskMode::Single,
                questions: vec![],
            }),
        ));
        let snap = p.snapshot_for(RingingChannel::Control, "s", 42);
        assert_eq!(snap.state["session_state"], "created");
        assert_eq!(snap.state["pending_interaction"]["id"], "i1");
        assert_eq!(snap.state_revision, 2);
    }

    #[test]
    fn conversation_snapshot_tracks_turn_lifecycle() {
        let mut p = SnapshotProjector::new();
        let ev = |ce: ConversationEvent| DomainEvent::Conversation(ce);
        p.apply(
            RingingChannel::Conversation,
            "s",
            &ev(ConversationEvent::TurnStarted {
                turn_id: "t1".into(),
                user_text: "hi".into(),
            }),
        );
        p.apply(
            RingingChannel::Conversation,
            "s",
            &ev(ConversationEvent::TurnCompleted {
                turn_id: "t1".into(),
                stop_reason: None,
                usage: None,
            }),
        );
        let snap = p.snapshot_for(RingingChannel::Conversation, "s", 10);
        assert_eq!(snap.state["active_turn"], serde_json::Value::Null);
        assert_eq!(snap.state["last_completed_turn"], "t1");
    }

    #[test]
    fn tool_snapshot_tracks_permission_then_finish() {
        let mut p = SnapshotProjector::new();
        let ev = |te: ToolEvent| DomainEvent::Tool(te);
        p.apply(
            RingingChannel::Tool,
            "s",
            &ev(ToolEvent::ToolPermissionRequested {
                tool_call_id: "c1".into(),
                turn_id: "t".into(),
                round_num: 0,
                tool_name: "exec".into(),
                reason: "r".into(),
                paths: vec![],
                category: qaqh_domain::PermissionCategory::Exec,
                level: 3,
                risk: qaqh_domain::PermissionRisk::High,
                consequence: "run".into(),
            }),
        );
        assert_eq!(
            p.snapshot_for(RingingChannel::Tool, "s", 0).state["pending_permission"],
            "c1"
        );
        p.apply(
            RingingChannel::Tool,
            "s",
            &ev(ToolEvent::ToolFinished {
                tool_call_id: "c1".into(),
                turn_id: "t".into(),
                round_num: 0,
                result: qaqh_domain::ToolResult::ok("ok"),
            }),
        );
        let snap = p.snapshot_for(RingingChannel::Tool, "s", 0);
        assert_eq!(snap.state["pending_permission"], serde_json::Value::Null);
        assert_eq!(snap.state["last_finished"], "c1");
    }

    #[test]
    fn activity_and_lifecycle_fold() {
        let mut p = SnapshotProjector::new();
        p.apply(
            RingingChannel::Control,
            "s",
            &DomainEvent::Control(ControlEvent::SessionActivityChanged {
                seed: "s".into(),
                state: ActivityState::WaitingUser,
                turn_id: Some("t".into()),
                seq: 1,
                updated_at: 0,
            }),
        );
        p.apply(
            RingingChannel::Control,
            "s",
            &DomainEvent::Control(ControlEvent::AgentLifecycleChanged {
                state: AgentLifecycleState::Ready,
            }),
        );
        let snap = p.snapshot_for(RingingChannel::Control, "s", 0);
        assert_eq!(snap.state["activity"], "waiting_user");
        assert_eq!(snap.state["agent_lifecycle"], "ready");
    }
}
