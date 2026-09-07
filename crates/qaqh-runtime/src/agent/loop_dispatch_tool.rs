//! agent::loop::dispatch_tool — Tool 命令分派（UI 调用/权限回执）。
//!
//! 由 `loop_core.rs` 拆分（Phase 2-5）：`impl Loop` 跨文件块，对外 API 不变。

use super::loop_core::Loop;
use super::types::*;

use super::engine_tool::PermissionDisposition;
use qaqh_domain::ToolCommand;

impl Loop {
    pub(super) fn on_tool(&mut self, command: ToolCommand, command_id: &str) {
        match command {
            ToolCommand::ToolInvoke {
                tool_call_id,
                name,
                action,
                args,
            } => {
                // C2：UI 快捷工具调用前清两处取消 token——Stop 之后
                // 快捷工具不再被残留取消态在 execution.rs 秒拒。
                self.cancel.clear();
                qaqh_workspace::clear_cancel();
                let mut ctx = RingContext {
                    agent: &mut self.session.agent,
                    emitter: &self.paced_emitter,
                    cancel: &self.cancel,
                    phase: &mut self.phase,
                    pending: &mut self.pending,
                    writer_dead: &self.writer_dead,
                    stats: &mut self.session.stats,
                    flow: &mut self.flow,
                };
                self.session.tool.handle_ui_tool_call(
                    &mut ctx,
                    &tool_call_id,
                    &name,
                    &action,
                    &args,
                );
            }
            ToolCommand::ToolPermissionRespond {
                tool_call_id,
                approved,
                trust_folder,
                ..
            } => {
                let mut ctx = RingContext {
                    agent: &mut self.session.agent,
                    emitter: &self.paced_emitter,
                    cancel: &self.cancel,
                    phase: &mut self.phase,
                    pending: &mut self.pending,
                    writer_dead: &self.writer_dead,
                    stats: &mut self.session.stats,
                    flow: &mut self.flow,
                };
                match self.session.tool.handle_permission_response(
                    &mut ctx,
                    &tool_call_id,
                    approved,
                    trust_folder,
                ) {
                    PermissionDisposition::Ignored => {
                        let _ = ctx;
                        self.emit_operation_failed(
                            command_id,
                            qaqh_domain::ErrorScope::Tool,
                            "interaction_not_found",
                            "tool permission request is no longer pending",
                        );
                    }
                    PermissionDisposition::UiHandled => {}
                    PermissionDisposition::LlmResolved { call_id, admitted } => {
                        let outcome = self.session.turn.handle_permission_resolved(
                            &mut ctx,
                            &mut self.session.tool,
                            &call_id,
                            admitted,
                        );
                        let _ = ctx;
                        self.apply_outcome(outcome);
                    }
                }
            }
        }
    }
}
