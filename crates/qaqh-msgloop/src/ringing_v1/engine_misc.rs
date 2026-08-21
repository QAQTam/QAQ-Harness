//! MiscEngine: undo, dashboard, mode.
//!
//! Handles undo, dashboard, and mode.
//!
//! # Undo consistency (cross-engine transaction)
//!
//! Undo is NOT just a message-store operation. It must also ensure
//! TurnEngine and ToolEngine are reset, because they may hold references
//! to the deleted turn (suspended state, pending approvals).
//! The Loop orchestrates this by calling `turn.reset()` and `tool.reset()`
//! BEFORE calling `handle_undo()`.

use crate::services::dashboard;
use crate::state::agent::AgentState;

use super::types::Emitter;

pub struct MiscEngine;

impl MiscEngine {
    pub fn new() -> Self {
        Self
    }
    pub fn reset(&mut self) {}

    // ── Undo ──
    ///
    /// Caller (Loop) MUST call `turn.reset()` and `tool.reset()` before
    /// calling this method, to ensure cross-engine consistency.
    pub fn handle_undo(&self, agent: &mut AgentState, turn_id: &str) {
        log::info!(
            "[MISC] UndoTurn {turn_id} — turns before: {}",
            agent.msg.turn_count()
        );
        if agent.msg.truncate_before_turn(turn_id) {
            log::info!(
                "[MISC] UndoTurn — truncated, turns after: {}",
                agent.msg.turn_count()
            );
            agent
                .msg
                .snapshot_full(&agent.config.model, &agent.config.reasoning_effort);
        } else {
            log::info!("[MISC] UndoTurn — no changes");
        }
    }

    // ── Dashboard ──

    pub fn emit_dashboard(&self, agent: &AgentState, emitter: &dyn Emitter) {
        // Write context stats to disk
        let (
            chat_text,
            thinking,
            tool_calls,
            tool_results,
            tools_schema,
            system_prompt,
            thinking_blocks,
            tool_call_blocks,
        ) = agent.msg.compute_context_stats(Some(&agent.tool_defs));
        let stats = serde_json::json!({
            "chat_text": chat_text, "thinking": thinking,
            "tool_calls": tool_calls, "tool_results": tool_results,
            "tools_schema": tools_schema, "system_prompt": system_prompt,
            "thinking_blocks": thinking_blocks, "tool_call_blocks": tool_call_blocks,
            "messages": 0,
        });
        // 统一数据源：上下文统计并入 meta.json（原 context_stats.json 退役）。
        qaqh_session::SessionManager::global().set_context_stats(&agent.session.seed, &stats);

        // Ringing 双发：DashboardUpdated（replaceable 覆盖）
        emitter.emit_domain(qaqh_domain::DomainEvent::Control(
            qaqh_domain::ControlEvent::DashboardUpdated {
                hp_connected: true,
                session_seed: agent.session.seed.clone(),
                tool_calls_total: 0,
                tool_failures: 0,
                current_phase: "single".into(),
                streaming: false,
            },
        ));
        emitter.emit_domain(qaqh_domain::DomainEvent::Control(
            qaqh_domain::ControlEvent::DashboardSnapshot {
                snapshot: dashboard::build_snapshot(agent.session.seed.clone()),
            },
        ));
    }

    // ── Mode ──

    pub fn set_mode(&self, agent: &mut AgentState, mode_str: &str) {
        let m: u8 = match mode_str {
            "plan" => 1,
            "code" => 2,
            _ => 0,
        };
        qaqh_workspace::runtime::set_mode(m);
        if !agent.session.seed.is_empty() {
            qaqh_session::SessionManager::global().persist_mode(&agent.session.seed, m);
        }
        log::info!("[MISC] mode set to {mode_str} (internal={m})");
    }
}
