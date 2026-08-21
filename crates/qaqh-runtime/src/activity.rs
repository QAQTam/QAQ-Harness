use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use qaqh_domain::{ActivityState, ControlEvent, DomainEvent};
use qaqh_proto::{SessionActivity, SessionActivityState};

use crate::RingingHub;

/// 活动状态双发：legacy `SessionActivity` 流 + Ringing `SessionActivityChanged`。
pub fn publish_activity(hub: Option<&RingingHub>, activity: &SessionActivity) {
    let Some(hub) = hub else {
        return;
    };
    let state = match activity.state {
        SessionActivityState::Starting => ActivityState::Starting,
        SessionActivityState::Idle => ActivityState::Idle,
        SessionActivityState::Working => ActivityState::Working,
        SessionActivityState::WaitingUser => ActivityState::WaitingUser,
        SessionActivityState::Disconnected => ActivityState::Disconnected,
    };
    let _ = hub.publish_with_causation(
        &activity.seed,
        DomainEvent::Control(ControlEvent::SessionActivityChanged {
            seed: activity.seed.clone(),
            state,
            turn_id: activity.turn_id.clone(),
            seq: activity.seq,
            updated_at: activity.updated_at,
        }),
        None,
    );
}

/// 领域事件 → `SessionActivityTracker::observe` 的事件类型映射（生产接线）。
///
/// tracker 的 Working→Idle 迁移此前只存在于测试中：daemon 侧除 spawn 的
/// Starting、输入预留的 Working、worker 断开的 Disconnected 之外没有任何
/// 生产路径更新状态，导致 session 列表/`session.activity` 查询在回合结束后
/// 永远显示 working/starting。本函数把 worker 事件流（registry stdout reader
/// 已转成领域事件）映射为 tracker 能消费的观察事件。
pub fn domain_activity_observe(event: &qaqh_domain::DomainEvent) -> Option<serde_json::Value> {
    use qaqh_domain::{
        AgentLifecycleState, ControlEvent, ConversationEvent, DomainEvent, ToolEvent,
    };
    match event {
        DomainEvent::Control(ControlEvent::AgentLifecycleChanged {
            state: AgentLifecycleState::Ready,
        }) => Some(serde_json::json!({ "type": "ready" })),
        DomainEvent::Conversation(ConversationEvent::TurnStarted { turn_id, .. }) => {
            Some(serde_json::json!({ "type": "turn_start", "turn_id": turn_id }))
        }
        DomainEvent::Conversation(ConversationEvent::TurnCompleted { .. }) => {
            Some(serde_json::json!({ "type": "turn_end" }))
        }
        DomainEvent::Conversation(ConversationEvent::TurnFailed { .. }) => {
            Some(serde_json::json!({ "type": "cancelled" }))
        }
        DomainEvent::Conversation(ConversationEvent::ConversationCancelled { .. }) => {
            Some(serde_json::json!({ "type": "cancelled" }))
        }
        DomainEvent::Conversation(ConversationEvent::CompactStarted { .. }) => {
            Some(serde_json::json!({ "type": "compact_start" }))
        }
        DomainEvent::Conversation(ConversationEvent::CompactFinished { .. }) => {
            Some(serde_json::json!({ "type": "compact_end" }))
        }
        DomainEvent::Control(ControlEvent::InteractionRequested { .. }) => {
            Some(serde_json::json!({ "type": "ask_user" }))
        }
        DomainEvent::Control(ControlEvent::InteractionResolved { .. }) => {
            Some(serde_json::json!({ "type": "ask_resolved" }))
        }
        DomainEvent::Control(ControlEvent::PlanReviewRequested { .. }) => {
            Some(serde_json::json!({ "type": "plan_submitted" }))
        }
        DomainEvent::Control(ControlEvent::PlanReviewResolved { .. }) => {
            Some(serde_json::json!({ "type": "plan_resolved" }))
        }
        DomainEvent::Tool(ToolEvent::ToolPermissionRequested { .. }) => {
            Some(serde_json::json!({ "type": "permission_request" }))
        }
        // 工具执行期间回合仍在跑：保持 Working（终态由 TurnCompleted 收敛）。
        DomainEvent::Tool(ToolEvent::ToolStarted { .. } | ToolEvent::ToolFinished { .. }) => {
            Some(serde_json::json!({ "type": "tool_results" }))
        }
        _ => None,
    }
}

#[derive(Clone, Default)]
pub struct SessionActivityTracker {
    inner: Arc<Mutex<HashMap<String, TrackedActivity>>>,
}

struct TrackedActivity {
    generation: u64,
    activity: SessionActivity,
}

impl SessionActivityTracker {
    pub fn begin(&self, seed: &str) -> (u64, SessionActivity) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let previous = inner.get(seed);
        let generation = previous.map_or(1, |value| value.generation.saturating_add(1));
        let seq = previous.map_or(1, |value| value.activity.seq.saturating_add(1));
        let activity = SessionActivity {
            seed: seed.to_string(),
            state: SessionActivityState::Starting,
            turn_id: None,
            seq,
            updated_at: now_millis(),
        };
        inner.insert(
            seed.to_string(),
            TrackedActivity {
                generation,
                activity: activity.clone(),
            },
        );
        (generation, activity)
    }

    pub fn observe(
        &self,
        seed: &str,
        generation: u64,
        event: &serde_json::Value,
    ) -> Option<SessionActivity> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let tracked = inner.get_mut(seed)?;
        if tracked.generation != generation {
            return None;
        }
        let event_type = event.get("type")?.as_str()?;
        // A user command may be queued while the agent is still Starting.
        // Its reservation changes the state to Working before the agent's
        // initialization Ready arrives. Do not let that Ready reopen the
        // session before the queued UserInput reaches TurnStart.
        if event_type == "ready"
            && tracked.activity.state == SessionActivityState::Working
            && tracked.activity.turn_id.is_none()
        {
            return None;
        }
        let current_turn = tracked.activity.turn_id.clone();
        let (state, turn_id) = match event_type {
            "ready" | "done" | "turn_end" | "cancelled" => (SessionActivityState::Idle, None),
            "shutdown_ack" => (SessionActivityState::Disconnected, None),
            "turn_start" => (
                SessionActivityState::Working,
                event
                    .get("turn_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
            ),
            "permission_request" | "ask_user" | "plan_submitted" => {
                (SessionActivityState::WaitingUser, current_turn)
            }
            "ask_resolved" | "plan_resolved" | "round_delta" | "round_complete"
            | "tool_results" | "tool_exec_delta" | "exec_progress" | "tool_call_preview"
            | "code_delta" | "compact_start" | "compact_delta" => {
                (SessionActivityState::Working, current_turn)
            }
            "compact_end" if current_turn.is_none() => (SessionActivityState::Idle, None),
            "compact_end" => (SessionActivityState::Working, current_turn),
            _ => return None,
        };
        if tracked.activity.state == state && tracked.activity.turn_id == turn_id {
            return None;
        }
        tracked.activity.state = state;
        tracked.activity.turn_id = turn_id;
        tracked.activity.seq = tracked.activity.seq.saturating_add(1);
        tracked.activity.updated_at = now_millis();
        Some(tracked.activity.clone())
    }

    pub fn get(&self, seed: &str) -> Option<SessionActivity> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(seed)
            .map(|tracked| tracked.activity.clone())
    }

    /// Reserve an idle session for a command before the agent's first event
    /// returns. This closes the window where a second command could observe
    /// stale Idle state after the first frame was already written to stdin.
    pub fn mark_working_if_idle(&self, seed: &str) -> Option<SessionActivity> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let tracked = inner.get_mut(seed)?;
        if tracked.activity.state != SessionActivityState::Idle {
            return None;
        }
        tracked.activity.state = SessionActivityState::Working;
        tracked.activity.turn_id = None;
        tracked.activity.seq = tracked.activity.seq.saturating_add(1);
        tracked.activity.updated_at = now_millis();
        Some(tracked.activity.clone())
    }

    pub fn mark_working_for_input(
        &self,
        seed: &str,
    ) -> Option<(SessionActivity, SessionActivityState)> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let tracked = inner.get_mut(seed)?;
        let previous = tracked.activity.state;
        if !matches!(
            previous,
            SessionActivityState::Starting | SessionActivityState::Idle
        ) {
            return None;
        }
        tracked.activity.state = SessionActivityState::Working;
        tracked.activity.turn_id = None;
        tracked.activity.seq = tracked.activity.seq.saturating_add(1);
        tracked.activity.updated_at = now_millis();
        Some((tracked.activity.clone(), previous))
    }

    pub fn restore_idle_if_unchanged(
        &self,
        seed: &str,
        expected_seq: u64,
    ) -> Option<SessionActivity> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let tracked = inner.get_mut(seed)?;
        if tracked.activity.seq != expected_seq
            || tracked.activity.state != SessionActivityState::Working
            || tracked.activity.turn_id.is_some()
        {
            return None;
        }
        tracked.activity.state = SessionActivityState::Idle;
        tracked.activity.seq = tracked.activity.seq.saturating_add(1);
        tracked.activity.updated_at = now_millis();
        Some(tracked.activity.clone())
    }

    pub fn restore_state_if_unchanged(
        &self,
        seed: &str,
        expected_seq: u64,
        previous: SessionActivityState,
    ) -> Option<SessionActivity> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let tracked = inner.get_mut(seed)?;
        if tracked.activity.seq != expected_seq
            || tracked.activity.state != SessionActivityState::Working
            || tracked.activity.turn_id.is_some()
        {
            return None;
        }
        tracked.activity.state = previous;
        tracked.activity.seq = tracked.activity.seq.saturating_add(1);
        tracked.activity.updated_at = now_millis();
        Some(tracked.activity.clone())
    }

    pub fn disconnect(&self, seed: &str, generation: u64) -> Option<SessionActivity> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let tracked = inner.get_mut(seed)?;
        if tracked.generation != generation
            || tracked.activity.state == SessionActivityState::Disconnected
        {
            return None;
        }
        tracked.activity.state = SessionActivityState::Disconnected;
        tracked.activity.turn_id = None;
        tracked.activity.seq = tracked.activity.seq.saturating_add(1);
        tracked.activity.updated_at = now_millis();
        Some(tracked.activity.clone())
    }

    pub fn snapshot(&self) -> Vec<SessionActivity> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut values: Vec<_> = inner.values().map(|value| value.activity.clone()).collect();
        values.sort_by(|a, b| a.seed.cmp(&b.seed));
        values
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idle_tracker(seed: &str) -> (SessionActivityTracker, u64) {
        let tracker = SessionActivityTracker::default();
        let (generation, _) = tracker.begin(seed);
        tracker
            .observe(seed, generation, &serde_json::json!({ "type": "ready" }))
            .expect("starting to idle");
        (tracker, generation)
    }

    #[test]
    fn idle_reservation_is_atomic_and_can_be_rolled_back() {
        let (tracker, _) = idle_tracker("seed");
        let reserved = tracker.mark_working_if_idle("seed").expect("reserve idle");

        assert_eq!(reserved.state, SessionActivityState::Working);
        assert!(tracker.mark_working_if_idle("seed").is_none());

        let rolled_back = tracker
            .restore_idle_if_unchanged("seed", reserved.seq)
            .expect("rollback unchanged reservation");
        assert_eq!(rolled_back.state, SessionActivityState::Idle);
    }

    #[test]
    fn rollback_does_not_overwrite_a_real_turn_transition() {
        let (tracker, generation) = idle_tracker("seed");
        let reserved = tracker.mark_working_if_idle("seed").expect("reserve idle");
        tracker
            .observe(
                "seed",
                generation,
                &serde_json::json!({ "type": "turn_start", "turn_id": "t1" }),
            )
            .expect("turn transition");

        assert!(
            tracker
                .restore_idle_if_unchanged("seed", reserved.seq)
                .is_none()
        );
        let current = tracker.get("seed").expect("current activity");
        assert_eq!(current.state, SessionActivityState::Working);
        assert_eq!(current.turn_id.as_deref(), Some("t1"));
    }

    #[test]
    fn compact_end_releases_a_manual_compact_reservation() {
        let (tracker, generation) = idle_tracker("seed");
        tracker.mark_working_if_idle("seed").expect("reserve idle");

        let completed = tracker
            .observe(
                "seed",
                generation,
                &serde_json::json!({
                    "type": "compact_end",
                    "summary_chars": 0,
                    "turns_compacted": 0,
                    "turns_removed": 0
                }),
            )
            .expect("compact completion");

        assert_eq!(completed.state, SessionActivityState::Idle);
        assert_eq!(completed.turn_id, None);
    }

    #[test]
    fn starting_user_input_reservation_survives_initial_ready() {
        let tracker = SessionActivityTracker::default();
        let (generation, _) = tracker.begin("seed");
        let (reserved, previous) = tracker
            .mark_working_for_input("seed")
            .expect("reserve starting session");

        assert_eq!(previous, SessionActivityState::Starting);
        assert!(
            tracker
                .observe("seed", generation, &serde_json::json!({ "type": "ready" }))
                .is_none()
        );
        assert_eq!(
            tracker.get("seed").expect("activity").state,
            SessionActivityState::Working
        );

        let turn = tracker
            .observe(
                "seed",
                generation,
                &serde_json::json!({ "type": "turn_start", "turn_id": "t1" }),
            )
            .expect("turn start");
        assert_eq!(turn.turn_id.as_deref(), Some("t1"));
        assert!(turn.seq > reserved.seq);
    }

    #[test]
    fn failed_starting_user_input_restores_starting_state() {
        let tracker = SessionActivityTracker::default();
        tracker.begin("seed");
        let (reserved, previous) = tracker
            .mark_working_for_input("seed")
            .expect("reserve starting session");

        let restored = tracker
            .restore_state_if_unchanged("seed", reserved.seq, previous)
            .expect("restore starting");
        assert_eq!(restored.state, SessionActivityState::Starting);
    }

    #[test]
    fn domain_events_drive_tracker_transitions_end_to_end() {
        use qaqh_domain::{
            AgentLifecycleState, ControlEvent, ConversationEvent, DomainEvent, ToolEvent,
        };

        let tracker = SessionActivityTracker::default();
        let (generation, _) = tracker.begin("seed");

        // worker ready → Idle（此前生产代码无任何路径迁移 Starting→Idle）
        let ready =
            domain_activity_observe(&DomainEvent::Control(ControlEvent::AgentLifecycleChanged {
                state: AgentLifecycleState::Ready,
            }))
            .expect("ready maps");
        assert_eq!(
            tracker
                .observe("seed", generation, &ready)
                .expect("ready")
                .state,
            SessionActivityState::Idle
        );

        // turn_start → Working + turn_id
        let start =
            domain_activity_observe(&DomainEvent::Conversation(ConversationEvent::TurnStarted {
                turn_id: "t1".into(),
                user_text: "hi".into(),
            }))
            .expect("turn_start maps");
        let activity = tracker
            .observe("seed", generation, &start)
            .expect("turn start");
        assert_eq!(activity.state, SessionActivityState::Working);
        assert_eq!(activity.turn_id.as_deref(), Some("t1"));

        // ask_user → WaitingUser
        let ask =
            domain_activity_observe(&DomainEvent::Control(ControlEvent::InteractionRequested {
                interaction_id: "i1".into(),
                turn_id: "t1".into(),
                mode: qaqh_domain::AskMode::Single,
                questions: vec![],
            }))
            .expect("ask maps");
        assert_eq!(
            tracker
                .observe("seed", generation, &ask)
                .expect("ask")
                .state,
            SessionActivityState::WaitingUser
        );

        // ask_resolved → Working（回合继续）
        let resolved =
            domain_activity_observe(&DomainEvent::Control(ControlEvent::InteractionResolved {
                interaction_id: "i1".into(),
                resolution: qaqh_domain::AskResolution::Answered,
            }))
            .expect("ask_resolved maps");
        assert_eq!(
            tracker
                .observe("seed", generation, &resolved)
                .expect("resolved")
                .state,
            SessionActivityState::Working
        );

        // turn_end → Idle（回合结束，turn_id 清空）
        let end = domain_activity_observe(&DomainEvent::Conversation(
            ConversationEvent::TurnCompleted {
                turn_id: "t1".into(),
                stop_reason: None,
                usage: None,
            },
        ))
        .expect("turn_end maps");
        let finished = tracker.observe("seed", generation, &end).expect("turn end");
        assert_eq!(finished.state, SessionActivityState::Idle);
        assert_eq!(finished.turn_id, None);

        // permission_request → WaitingUser
        let perm =
            domain_activity_observe(&DomainEvent::Tool(ToolEvent::ToolPermissionRequested {
                tool_call_id: "c1".into(),
                turn_id: "t2".into(),
                round_num: 0,
                tool_name: "exec".into(),
                reason: "r".into(),
                paths: vec![],
                category: qaqh_domain::PermissionCategory::Exec,
                level: 3,
                risk: qaqh_domain::PermissionRisk::High,
                consequence: "run".into(),
            }))
            .expect("permission maps");
        assert_eq!(
            tracker
                .observe("seed", generation, &perm)
                .expect("perm")
                .state,
            SessionActivityState::WaitingUser
        );

        // 无关事件不产生观察事件
        let none = domain_activity_observe(&DomainEvent::Tool(ToolEvent::ToolNotice {
            tool_call_id: None,
            level: qaqh_domain::NoticeLevel::Info,
            message: "m".into(),
        }));
        assert!(none.is_none());
    }
}
