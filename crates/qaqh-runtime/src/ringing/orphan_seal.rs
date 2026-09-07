//! ringing::orphan_seal — 孤儿收尾（running turn seal + channel state seal + 全量 seal）。
//!
//! 由 `hub.rs` 拆分（Phase 2-6）：`impl RingingHub` 跨文件块，对外 API 不变。

use super::hub::RingingHub;
use qaqh_domain::{
    AskResolution, CompactStatus, ControlEvent, ConversationEvent, DomainEvent, RingingChannel,
    TimelineBlockState, TimelineFailure, TimelineTurnState, ToolEvent,
};
use qaqh_types::tool_result::ToolResult;

impl RingingHub {
    /// 收尾孤儿 running turn。daemon 重启或 worker 重新 spawn 后，timeline 中
    /// 任何未 seal 的 turn 都没有存活生产者（典型场景：工具调用未返回 result
    /// 时进程被杀）。若不 seal，前端会永远把它投影为 running，stop/send 按钮
    /// 卡死在 stop 且新消息无法发送——这是"重启 daemon/前端都无法再发送新
    /// 消息"的根因。
    ///
    /// seal 顺序遵循 TimelineAppender 契约：先 seal 全部 open block，再 seal
    /// 全部未 seal round（is_final=true，这是该 turn 的最后一轮），最后将 turn
    /// seal 为 Cancelled。幂等：已 seal 的 turn 直接跳过。返回是否有变更。
    pub fn seal_orphan_running_turns(&self, seed: &str) -> bool {
        let mut appender = self.timeline.lock().unwrap_or_else(|e| e.into_inner());
        let Some(snapshot) = appender.snapshot(seed) else {
            return false;
        };
        let mut changed = false;
        for turn in snapshot.turns.iter().filter(|turn| !turn.sealed) {
            for round in &turn.rounds {
                for block in &round.blocks {
                    if block.state == TimelineBlockState::Sealed {
                        continue;
                    }
                    match appender.seal_block(seed, &turn.turn_id, round.round_num, &block.block_id)
                    {
                        Ok(_) => changed = true,
                        Err(error) => log::warn!(
                            "[timeline] orphan seal block failed for {seed} {}: {error}",
                            block.block_id
                        ),
                    }
                }
            }
            for round in &turn.rounds {
                if round.sealed {
                    continue;
                }
                match appender.seal_round(seed, &turn.turn_id, round.round_num, true) {
                    Ok(_) => changed = true,
                    Err(error) => log::warn!(
                        "[timeline] orphan seal round failed for {seed} {}/{}: {error}",
                        turn.turn_id,
                        round.round_num
                    ),
                }
            }
            match appender.seal_turn_with_state(
                seed,
                &turn.turn_id,
                TimelineTurnState::Cancelled,
                Some(TimelineFailure {
                    code: "daemon_restart_interrupted".into(),
                    message:
                        "Daemon restarted while this turn was running; the turn was interrupted and the session is ready for new input."
                            .into(),
                }),
            ) {
                Ok(_) => changed = true,
                Err(error) => log::warn!(
                    "[timeline] orphan seal turn failed for {seed} {}: {error}",
                    turn.turn_id
                ),
            }
        }
        changed
    }

    pub fn seal_orphan_channel_state(&self, seed: &str, force: bool) -> bool {
        // B9/H3 liveness gate：force=false 的 bootstrap 路径在 worker 仍
        // 存活时整体跳过——活 worker 的 running/pending 状态不是孤儿。
        if !force
            && self
                .live_workers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(seed)
        {
            log::info!("[ringing] worker alive for {seed}; skipping bootstrap orphan seal");
            return false;
        }

        let mut changed = false;

        // 1) conversation：active_turn 无终态（journal 重放后仍有值）→ 取消。
        let conv = self.snapshot(RingingChannel::Conversation, seed);
        if let Some(turn_id) = conv
            .state
            .get("active_turn")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            log::info!(
                "[ringing] sealing orphan active turn {turn_id} for {seed} (no terminal event)"
            );
            let _ = self.publish_with_causation(
                seed,
                DomainEvent::Conversation(ConversationEvent::ConversationCancelled {
                    turn_id: Some(turn_id.to_string()),
                }),
                None,
            );
            changed = true;
        }

        // 2) conversation compact：CompactStarted 无 CompactFinished → 失败。
        //    压缩 worker 的网络请求与结果仅存于旧进程内存，daemon/worker
        //    恢复后不可能继续；必须经正常终态事件收敛 journal、snapshot 和 SSE。
        let conv = self.snapshot(RingingChannel::Conversation, seed);
        if conv.state.get("compact_status").and_then(|v| v.as_str()) == Some("running") {
            let compact_id = conv
                .state
                .get("compact_id")
                .and_then(|v| v.as_str())
                .filter(|value| !value.is_empty())
                .unwrap_or("orphan-compact")
                .to_string();
            log::info!(
                "[ringing] sealing orphan compact {compact_id} for {seed} (worker operation cannot resume)"
            );
            let _ = self.publish_with_causation(
                seed,
                DomainEvent::Conversation(ConversationEvent::CompactFinished {
                    compact_id,
                    status: CompactStatus::Failed,
                    summary_chars: Some(0),
                    turns_compacted: Some(0),
                    turns_removed: Some(0),
                }),
                None,
            );
            changed = true;
        }

        // 3) tool：running 列表 + pending_permission 无 ToolFinished 终态 → 取消。
        //    兼容旧投影的字符串数组与当前的对象数组两种格式。
        let tool = self.snapshot(RingingChannel::Tool, seed);
        let mut orphans: Vec<(String, String, u32)> = Vec::new();
        if let Some(running) = tool.state.get("running").and_then(|v| v.as_array()) {
            for entry in running {
                match entry {
                    serde_json::Value::String(id) => {
                        orphans.push((id.clone(), String::new(), 0));
                    }
                    serde_json::Value::Object(obj) => {
                        if let Some(id) = obj.get("tool_call_id").and_then(|v| v.as_str()) {
                            orphans.push((
                                id.to_string(),
                                obj.get("turn_id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                                obj.get("round_num").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                            ));
                        }
                    }
                    _ => {}
                }
            }
        }
        if let Some(id) = tool
            .state
            .get("pending_permission")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            && !orphans
                .iter()
                .any(|(tool_call_id, _, _)| tool_call_id == id)
        {
            orphans.push((id.to_string(), String::new(), 0));
        }
        for (tool_call_id, turn_id, round_num) in orphans {
            log::info!("[ringing] sealing orphan tool {tool_call_id} for {seed} (no ToolFinished)");
            let _ = self.publish_with_causation(
                seed,
                DomainEvent::Tool(ToolEvent::ToolFinished {
                    tool_call_id,
                    turn_id,
                    round_num,
                    result: ToolResult::cancelled(
                        "Agent restarted before the tool returned a result",
                    ),
                }),
                None,
            );
            changed = true;
        }

        // 4) control：pending_interaction 无 InteractionResolved → 关闭（Dismissed）。
        //    守卫：当前进程发布、仍在等待用户响应的活交互不 seal——bootstrap
        //    路径（force=false）会误杀 1ms 前刚发布的 ask（「ask 弹不出」根因）；
        //    force=true（worker 死亡/重启的 registry 收尾路径）无视守卫强制收尾。
        let control = self.snapshot(RingingChannel::Control, seed);
        if let Some(id) = control
            .state
            .get("pending_interaction")
            .and_then(|v| v.get("id"))
            .and_then(|v| v.as_str())
        {
            let is_live = self
                .live_interactions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(seed)
                .is_some_and(|cur| cur == id);
            if is_live && !force {
                log::info!(
                    "[ringing] keeping live interaction {id} for {seed} (awaiting user response)"
                );
            } else {
                log::info!("[ringing] sealing orphan interaction {id} for {seed} (no resolution)");
                let _ = self.publish_with_causation(
                    seed,
                    DomainEvent::Control(ControlEvent::InteractionResolved {
                        interaction_id: id.to_string(),
                        resolution: AskResolution::Dismissed,
                    }),
                    None,
                );
                // B9/H3c：条件删除——仅当活表仍指向被收尾的 id 才清除；
                // 并发发布的新 ask 可能已注册了不同 id，不能误抹。
                let mut live = self
                    .live_interactions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if live.get(seed).is_some_and(|cur| cur == id) {
                    live.remove(seed);
                }
                drop(live);
                changed = true;
            }
        }

        changed
    }

    /// 优雅关闭收尾：对所有已知 timeline seed 执行孤儿收尾（timeline +
    /// 三频道投影）。正常路径下 worker 已优雅退出并自行 seal（terminal
    /// intent 同步落盘），此处兜底 worker 超时被杀 / 未收尾的场景——
    /// 退出时不留孤儿，安装器更新后重启不再出现 daemon_restart_interrupted。
    ///
    /// 只覆盖已加载 seed 的当前状态：磁盘上未加载的 seed 由下次
    /// `ensure_timeline_loaded` 启动时收尾（懒加载路径自带孤儿 seal）。
    pub fn seal_all_orphans(&self) {
        let seeds: Vec<String> = self
            .disk_timeline_seeds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect();
        for seed in &seeds {
            if self.seal_orphan_running_turns(seed) {
                log::info!("[timeline] sealed orphan turn(s) for {seed} at shutdown");
            }
            if self.seal_orphan_channel_state(seed, true) {
                log::info!("[ringing] sealed orphan channel state for {seed} at shutdown");
            }
        }
    }
}
