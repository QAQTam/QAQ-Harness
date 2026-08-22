use crate::effect::{Effect, PendingTool, ToolExecRequest, ToolExecutorFn};
use qaqh_session::SessionManager;
use qaqh_types::{Message, ToolDef};

/// Tool results are finalized exactly once, at storage time — but the shaping
/// itself (truncation / folding) now happens at the TOOL side
/// (`qaqh-workspace::tool_side_fold`) before results reach this store. The
/// stored message therefore IS the final form: the same bytes are rendered on
/// every subsequent request, keeping KV-cache prefixes stable across rounds
/// and turns.

#[derive(Debug, Clone)]
pub struct Step {
    pub assistant: Message,
    pub tool_results: Vec<Message>,
}

impl Step {
    pub fn new(assistant: Message) -> Self {
        Self {
            assistant,
            tool_results: Vec::new(),
        }
    }

    pub fn assistant_tool_ids(&self) -> Vec<String> {
        self.assistant
            .content
            .iter()
            .filter_map(|b| {
                if let qaqh_types::ContentBlock::ToolUse { id, .. } = b {
                    Some(id.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn tool_result_has_id(&self, id: &str) -> bool {
        self.tool_results.iter().any(|tr| tr.content.iter().any(|b| matches!(b, qaqh_types::ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id)))
    }

    pub fn has_tool_call(&self, id: &str) -> bool {
        self.assistant_tool_ids().iter().any(|tid| tid == id)
    }

    pub fn all_tools_satisfied(&self) -> bool {
        let ids = self.assistant_tool_ids();
        if ids.is_empty() {
            return true;
        }
        ids.iter().all(|id| self.tool_result_has_id(id))
    }

    pub fn pending_tools(&self) -> Vec<PendingTool> {
        self.assistant
            .content
            .iter()
            .filter_map(|b| {
                if let qaqh_types::ContentBlock::ToolUse { id, name, input } = b {
                    Some(PendingTool {
                        id: id.clone(),
                        name: name.clone(),
                        args: input.clone(),
                    })
                } else {
                    None
                }
            })
            .collect()
    }

    fn has_tool_use(&self) -> bool {
        self.assistant
            .content
            .iter()
            .any(|b| matches!(b, qaqh_types::ContentBlock::ToolUse { .. }))
    }
}

#[derive(Debug, Clone)]
pub struct Turn {
    pub user: Message,
    pub steps: Vec<Step>,
}

impl Turn {
    pub fn new(user: Message) -> Self {
        Self {
            user,
            steps: Vec::new(),
        }
    }

    pub fn find_step_for_mut(&mut self, tool_call_id: &str) -> Option<&mut Step> {
        self.steps
            .iter_mut()
            .find(|s| s.has_tool_call(tool_call_id))
    }
}

#[allow(clippy::type_complexity)]
pub struct MessageStore {
    seed: String,
    system_messages: Vec<Message>,
    /// Trailing injections (persisted): subagent reports, skills envelopes.
    /// Written exactly once at arrival; never re-injected, never removed while
    /// they are in the live context (undo=Keep). Once the model consumes an
    /// injection it becomes ordinary history — later turns/steps append after
    /// it. On compact, injections in the folded region are folded with their
    /// turns (their text is already in the summary input). Serialized into the
    /// model context in WRITE order together with turns (see
    /// [`Self::flat_in_write_order`]), so an injection keeps the position it
    /// had when it was written — later turns and steps append AFTER it, and it
    /// becomes normal history instead of being re-presented as a fresh user
    /// message on every request.
    trailing_messages: Vec<Message>,
    turns: Vec<Turn>,
    cancelled: bool,
    tool_executor: Option<ToolExecutorFn>,
    /// Number of earliest turns that have been compacted (skipped in LLM context).
    compact_skip: usize,
    /// True once the store is backed by a separate compact checkpoint instead
    /// of treating `messages.jsonl` as its active context.
    has_compact_context: bool,
    /// Next message ID to assign (monotonic per session).
    next_msg_id: u64,
    /// Next externally visible turn sequence. Unlike `turns.len()`, this never
    /// moves backwards when old turns are replaced by a compact checkpoint.
    next_turn_seq: u64,
    /// Monotonic in-memory context generation. Background compaction captures
    /// this value and must only apply its result while the generation matches.
    context_revision: u64,
    /// If true, save_msg is a no-op — used during from_messages replay.
    replaying: bool,
    /// Messages assigned msg_id but not yet flushed to disk.
    pending_save: Vec<Message>,
    /// If true, skip all disk persistence. Used by subagents (disposable workers).
    ephemeral: bool,
}

impl std::fmt::Debug for MessageStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessageStore")
            .field("seed", &self.seed)
            .field("turns", &self.turns.len())
            .field("cancelled", &self.cancelled)
            .field("has_executor", &self.tool_executor.is_some())
            .field("compact_skip", &self.compact_skip)
            .field("next_msg_id", &self.next_msg_id)
            .finish()
    }
}

impl Clone for MessageStore {
    fn clone(&self) -> Self {
        Self {
            seed: self.seed.clone(),
            system_messages: self.system_messages.clone(),
            trailing_messages: self.trailing_messages.clone(),
            turns: self.turns.clone(),
            cancelled: self.cancelled,
            tool_executor: None,
            compact_skip: self.compact_skip,
            has_compact_context: self.has_compact_context,
            next_msg_id: self.next_msg_id,
            next_turn_seq: self.next_turn_seq,
            context_revision: self.context_revision,
            replaying: false,
            pending_save: Vec::new(),
            ephemeral: self.ephemeral,
        }
    }
}

impl MessageStore {
    pub fn new(seed: &str) -> Self {
        Self {
            seed: seed.to_string(),
            system_messages: Vec::new(),
            trailing_messages: Vec::new(),
            turns: Vec::new(),
            cancelled: false,
            tool_executor: None,
            compact_skip: 0,
            has_compact_context: false,
            next_msg_id: 1,
            next_turn_seq: 1,
            context_revision: 0,
            replaying: false,
            pending_save: Vec::new(),
            ephemeral: false,
        }
    }

    /// Create a MessageStore that never persists to disk (subagent / disposable worker).
    pub fn new_ephemeral(seed: &str) -> Self {
        let mut s = Self::new(seed);
        s.ephemeral = true;
        s
    }

    pub fn seed(&self) -> &str {
        &self.seed
    }

    pub fn context_revision(&self) -> u64 {
        self.context_revision
    }

    pub fn cancel(&mut self) {
        self.cancelled = true;
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    /// Assign msg_id and buffer for batched persistence.
    /// Flushed to disk via [`flush_meta`].
    /// No-op in ephemeral mode (returns None).
    ///
    /// Returns the assigned write-order id so callers can stamp the in-memory
    /// message: `msg_id` doubles as the store's write sequence, which
    /// [`Self::flat_in_write_order`] uses to merge turns and trailing
    /// injections in arrival order.
    fn save_msg(&mut self, msg: &Message) -> Option<u64> {
        if self.ephemeral {
            return None;
        }
        let mut m = msg.clone();
        m.msg_id = Some(self.next_msg_id);
        self.next_msg_id += 1;
        let id = m.msg_id;
        if !self.replaying {
            self.pending_save.push(m);
        }
        id
    }

    /// Write buffered messages to JSONL, then update meta.json + index.
    /// No-op if the session seed has not been initialized yet (empty seed),
    /// or if ephemeral mode is enabled.
    pub fn flush_meta(&mut self, model: &str, effort: &str) {
        if self.seed.is_empty() || self.ephemeral {
            return;
        }
        let turn_count = self.turns.len();
        if !self.pending_save.is_empty() {
            SessionManager::global().save_append(
                &self.seed,
                &self.pending_save,
                model,
                Some(effort),
                self.compact_skip,
                turn_count,
            );
            self.pending_save.clear();
            if self.has_compact_context {
                SessionManager::global().update_compact_context(&self.seed, &self.to_vec());
            }
        } else {
            SessionManager::global().update_meta(
                &self.seed,
                model,
                Some(effort),
                self.compact_skip,
                turn_count,
            );
        }
    }

    pub fn push_system(&mut self, msg: Message) -> Effect {
        debug_assert_eq!(msg.role, "system", "push_system requires role=system");
        // Guard: skip if an identical system message already exists.
        // This prevents double-injection when lifecycle paths are called
        // multiple times (e.g. create_session after a failed resume).
        let new_text = msg
            .content
            .iter()
            .find_map(|b| match b {
                qaqh_types::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .unwrap_or("");
        if !new_text.is_empty()
            && self.system_messages.iter().any(|m| {
                m.content.iter().any(|b| match b {
                    qaqh_types::ContentBlock::Text { text } => text == new_text,
                    _ => false,
                })
            })
        {
            return Effect::None;
        }
        let mut msg = msg;
        let id = self.save_msg(&msg);
        if let Some(id) = id {
            msg.msg_id = Some(id);
        }
        self.system_messages.push(msg);
        self.context_revision = self.context_revision.saturating_add(1);
        Effect::None
    }

    /// Remove all system messages whose first text block starts with the
    /// given prefix. Used to replace catalog messages without relying on
    /// the [QAQH_SKILL_V1] format.
    pub fn remove_system_messages_by_prefix(&mut self, prefix: &str) {
        let before = self.system_messages.len();
        self.system_messages.retain(|message| {
            let text = message
                .content
                .iter()
                .find_map(|block| match block {
                    qaqh_types::ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .unwrap_or("");
            !text.starts_with(prefix)
        });
        if self.system_messages.len() != before {
            self.context_revision = self.context_revision.saturating_add(1);
        }
    }

    /// Append a trailing injection, once.
    ///
    /// Idempotent: an identical message is skipped, so repeated sync calls on
    /// an unchanged activation set never duplicate. The message is persisted
    /// (save_msg, which also assigns the write-order msg_id) and stays in
    /// place for the rest of the session — never re-injected, never removed —
    /// keeping the request prefix byte-stable. Context serialization merges
    /// trailing injections with turns in write order
    /// ([`Self::flat_in_write_order`]), so this message lands at the position
    /// where it was written and later turns/steps append after it.
    pub fn push_trailing_system(&mut self, msg: Message) -> bool {
        let new_text = msg
            .content
            .iter()
            .find_map(|b| match b {
                qaqh_types::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .unwrap_or("");
        debug_assert!(
            msg.role == "system"
                || msg.role == "developer"
                || (msg.role == "user" && msg.name.as_deref() == Some("subagent"))
                || (msg.role == "user" && new_text.starts_with("[SUBAGENT ")),
            "push_trailing_system requires role=system|developer|user(subagent), got role={} name={:?}",
            msg.role,
            msg.name
        );
        if !new_text.is_empty()
            && self.trailing_messages.iter().any(|m| {
                m.content.iter().any(|b| match b {
                    qaqh_types::ContentBlock::Text { text } => text == new_text,
                    _ => false,
                })
            })
        {
            return false;
        }
        let mut msg = msg;
        let id = self.save_msg(&msg);
        if let Some(id) = id {
            msg.msg_id = Some(id);
        }
        self.trailing_messages.push(msg);
        self.context_revision = self.context_revision.saturating_add(1);
        true
    }

    /// Access the persisted trailing system messages (injection history).
    pub fn trailing_messages(&self) -> &[Message] {
        &self.trailing_messages
    }

    pub fn push_user(&mut self, text: &str) -> Effect {
        if !self.replaying {
            if let Some(turn) = self.turns.last_mut() {
                if let Some(step) = turn.steps.last_mut() {
                    auto_complete_unfulfilled(
                        step,
                        "[CANCELLED] Tool was not executed (user interrupted).",
                    );
                }
            }
        }
        let mut msg = Message::user(text);
        let id = self.save_msg(&msg);
        if let Some(id) = id {
            msg.msg_id = Some(id);
        }
        self.turns.push(Turn::new(msg));
        self.context_revision = self.context_revision.saturating_add(1);
        Effect::None
    }

    /// Push a system-role message as a standalone turn (e.g. sub-agent result
    /// injection). Persisted like a user turn (`save_msg`), so `to_vec()`
    /// emits a mid-stream `system` message that OpenAI/Responses both accept.
    /// Callers must keep the `[SUBAGENT ...]` tag in `text` so the model can
    /// distinguish injected data from system instructions.
    pub fn push_system_input(&mut self, text: &str) -> Effect {
        if !self.replaying {
            if let Some(turn) = self.turns.last_mut() {
                if let Some(step) = turn.steps.last_mut() {
                    auto_complete_unfulfilled(
                        step,
                        "[CANCELLED] Tool was not executed (system injection arrived).",
                    );
                }
            }
        }
        let mut msg = Message::system(text);
        let id = self.save_msg(&msg);
        if let Some(id) = id {
            msg.msg_id = Some(id);
        }
        self.turns.push(Turn::new(msg));
        self.context_revision = self.context_revision.saturating_add(1);
        Effect::None
    }

    /// Add an image block to the last user message (the most recent turn's user message).
    pub fn push_image_to_last_user(&mut self, mime_type: &str, data: &str) {
        if let Some(turn) = self.turns.last_mut() {
            turn.user.content.push(qaqh_types::ContentBlock::Image {
                mime_type: mime_type.to_string(),
                data: data.to_string(),
            });
            self.context_revision = self.context_revision.saturating_add(1);
        }
    }

    pub fn push_assistant(&mut self, msg: Message) -> Effect {
        debug_assert_eq!(
            msg.role, "assistant",
            "push_assistant requires role=assistant"
        );

        if self.turns.is_empty() {
            log::error!(
                "push_assistant: no turn exists — assistant response without user input. Dropping."
            );
            return Effect::None;
        }

        if !self.replaying {
            if let Some(step) = self.turns.last_mut().and_then(|t| t.steps.last_mut()) {
                auto_complete_unfulfilled(
                    step,
                    "[AUTO] Tool was not executed before next assistant response.",
                );
            }
        }

        let mut msg = msg;
        let id = self.save_msg(&msg);
        if let Some(id) = id {
            msg.msg_id = Some(id);
        }
        let step = Step::new(msg);
        let has_tools = step.has_tool_use();
        self.turns
            .last_mut()
            .expect("checked above")
            .steps
            .push(step);
        self.context_revision = self.context_revision.saturating_add(1);

        if has_tools {
            Effect::None
        } else {
            Effect::TurnComplete
        }
    }

    pub fn push_tool_result(&mut self, tool_call_id: &str, result: &str, success: bool) -> Effect {
        if self.push_tool_result_inner(tool_call_id, result, success, None) {
            self.context_revision = self.context_revision.saturating_add(1);
        }

        if let Some(turn) = self.turns.last() {
            if let Some(step) = turn.steps.last() {
                if step.all_tools_satisfied() {
                    return if step.pending_tools().is_empty() {
                        Effect::TurnComplete
                    } else {
                        Effect::None
                    };
                }
            }
        }
        Effect::None
    }

    pub fn push_tool_results_batch(&mut self, results: &[(String, String, bool)]) -> Effect {
        let mut changed = false;
        for (tc_id, result, success) in results {
            changed |= self.push_tool_result_inner(tc_id, result, *success, None);
        }
        if changed {
            self.context_revision = self.context_revision.saturating_add(1);
        }

        if let Some(turn) = self.turns.last() {
            if let Some(step) = turn.steps.last() {
                if step.all_tools_satisfied() {
                    return if step.pending_tools().is_empty() {
                        Effect::TurnComplete
                    } else {
                        Effect::None
                    };
                }
            }
        }
        Effect::None
    }

    fn push_tool_result_inner(
        &mut self,
        tool_call_id: &str,
        result: &str,
        success: bool,
        diff: Option<String>,
    ) -> bool {
        // 工具结果已在工具侧定型（qaqh-workspace::tool_side_fold）：
        // 存储的就是最终形态，message 层不再改写。
        let final_result = result.to_string();
        let mut tool_msg = Message::tool(tool_call_id, &final_result, success);
        // 展示平面 diff 附着在结构化 ToolResult 上（project_for_model 不携带，
        // 模型上下文不受影响）；供 last_step_tool_results → timeline 消费。
        if let Some(diff) = diff {
            if let Some(qaqh_types::ContentBlock::ToolResult { result, .. }) =
                tool_msg.content.first_mut()
            {
                result.diff = Some(diff);
            }
        }

        for turn in self.turns.iter_mut().rev() {
            if let Some(step) = turn.find_step_for_mut(tool_call_id) {
                if !step.tool_result_has_id(tool_call_id) {
                    step.tool_results.push(tool_msg.clone());
                    // push 之后不再使用 step/turn（NLL 释放借用），再保存。
                    let id = self.save_msg(&tool_msg);
                    if let Some(id) = id {
                        // 新借用回写：最后压入的 tool_result 即本条消息。
                        for t in self.turns.iter_mut().rev() {
                            if let Some(s) = t.find_step_for_mut(tool_call_id) {
                                if let Some(tr) = s.tool_results.last_mut() {
                                    tr.msg_id = Some(id);
                                }
                                break;
                            }
                        }
                    }
                    return true;
                }
                return false;
            }
        }
        if let Some(turn) = self.turns.last_mut() {
            if let Some(step) = turn.steps.last_mut() {
                if step.has_tool_call(tool_call_id) {
                    log::warn!(
                        "push_tool_result: tool_result {} matched by last-step fallback — appending",
                        tool_call_id
                    );
                    step.tool_results.push(tool_msg.clone());
                    let id = self.save_msg(&tool_msg);
                    if let Some(id) = id {
                        for t in self.turns.iter_mut().rev() {
                            if let Some(s) = t.find_step_for_mut(tool_call_id) {
                                if let Some(tr) = s.tool_results.last_mut() {
                                    tr.msg_id = Some(id);
                                }
                                break;
                            }
                        }
                    }
                    return true;
                }
                log::error!(
                    "push_tool_result: orphan tool_result {} — last step does not own this call_id, dropped",
                    tool_call_id
                );
                return false;
            }
        }
        log::error!(
            "push_tool_result: orphan tool_result {} — nowhere to place, dropped",
            tool_call_id
        );
        false
    }

    pub fn replace_tool_result(&mut self, tool_call_id: &str, result: &str, success: bool) {
        // Same as push path: stored bytes are final (tool side already shaped).
        let final_result = result.to_string();

        for turn in self.turns.iter_mut().rev() {
            if let Some(step) = turn.find_step_for_mut(tool_call_id) {
                // 保留被替换消息的 msg_id：它是写入顺序键（trailing 合并依赖），
                // 替换后消息必须留在原时间位置。
                let old_id = step
                    .tool_results
                    .iter()
                    .find(|tr| {
                        tr.content.iter().any(|b| {
                            matches!(b, qaqh_types::ContentBlock::ToolResult { tool_use_id, .. }
                                if tool_use_id == tool_call_id)
                        })
                    })
                    .and_then(|tr| tr.msg_id);
                step.tool_results.retain(|tr| !tr.content.iter().any(|b| matches!(b, qaqh_types::ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == tool_call_id)));
                let mut replacement = Message::tool(tool_call_id, &final_result, success);
                replacement.msg_id = old_id;
                step.tool_results.push(replacement);
                self.context_revision = self.context_revision.saturating_add(1);
                return;
            }
        }
        log::error!(
            "replace_tool_result: tool_call_id {} not found in any turn",
            tool_call_id
        );
    }

    /// Flatten turns (optionally skipping the first `skip_turns`) and trailing
    /// injections into a single message stream ordered by WRITE time.
    ///
    /// `msg_id` is assigned monotonically by [`Self::save_msg`] at write time
    /// to every persisted message (user turns, assistant steps, tool results,
    /// trailing injections), so id order == arrival order. `turns` and
    /// `trailing_messages` are each internally id-ascending; sorting the merged
    /// list by id reproduces the exact arrival order — a subagent report that
    /// landed mid-turn sits between that turn's earlier and later steps, and
    /// once the model acknowledges it the report is followed by the ack, i.e.
    /// it becomes history instead of re-reading as a fresh user message on
    /// every request.
    ///
    /// Fallback: if any message lacks a msg_id (ephemeral stores), emit turns
    /// first then trailing — the legacy order. This keeps ephemeral/subagent
    /// workers (which never persist ids) byte-compatible with the old layout.
    fn flat_in_write_order(&self, skip_turns: usize) -> Vec<Message> {
        let mut items: Vec<(Option<u64>, Message)> = Vec::new();
        for turn in self.turns.iter().skip(skip_turns) {
            items.push((turn.user.msg_id, turn.user.clone()));
            for step in &turn.steps {
                items.push((step.assistant.msg_id, step.assistant.clone()));
                for tr in &step.tool_results {
                    items.push((tr.msg_id, tr.clone()));
                }
            }
        }
        for m in &self.trailing_messages {
            items.push((m.msg_id, m.clone()));
        }
        if items.iter().all(|(id, _)| id.is_some()) {
            // Stable sort; msg_ids are unique per session, so this is a total
            // order. O(n log n) on a few hundred messages is negligible next
            // to the provider round-trip it serves.
            items.sort_by_key(|(id, _)| id.unwrap_or(u64::MAX));
        }
        items.into_iter().map(|(_, m)| m).collect()
    }

    pub fn build_context_for_gate(&self, annotations: &[String]) -> Vec<Message> {
        let mut full: Vec<Message> = {
            let mut v = Vec::new();
            v.extend(self.system_messages.clone());
            // Turns + trailing injections merged in WRITE order (msg_id): a
            // subagent report that landed mid-turn keeps that position — the
            // model's acknowledgement of it is written after the report, so it
            // serializes after it and the report becomes history. (Previously
            // trailing was pinned after ALL turns: every request ended with
            // the report as the last message, which the model re-read as a
            // freshly-arrived user message on each round.)
            v.extend(self.flat_in_write_order(self.compact_skip));
            v
        };

        if !annotations.is_empty() {
            let ann_text = annotations.join("\n");
            // Inject into the FIRST real user message: its position is fixed for
            // the lifetime of the context, so the [Environment] block never
            // moves between user messages. (Injecting into the last user message
            // made turn-1's message render differently once turn 2 arrived,
            // breaking the whole prefix cache at the first user message — cache
            // hits collapsed to the system prefix only.) In write-order
            // serialization a trailing subagent report could theoretically
            // precede the first user turn (injection into an auto-created
            // session), so skip name="subagent" messages — annotations belong
            // to the human user's message, not to an injected report.
            if let Some(first_user) = full
                .iter_mut()
                .find(|m| m.role == "user" && m.name.as_deref() != Some("subagent"))
            {
                let existing = first_user.content.iter_mut().find_map(|b| {
                    if let qaqh_types::ContentBlock::Text { text } = b {
                        Some(text)
                    } else {
                        None
                    }
                });
                if let Some(text) = existing {
                    let original = text.clone();
                    *text = format!("[Environment]\n{}\n\n[UserMessage]\n{}", ann_text, original);
                } else {
                    first_user
                        .content
                        .push(qaqh_types::ContentBlock::text(&ann_text));
                }
            }
        }

        full
    }

    /// Get pending tools from the last step (for manual execution with streaming).
    pub fn get_last_step_pending(&self) -> Vec<PendingTool> {
        let step = match self.turns.last().and_then(|t| t.steps.last()) {
            Some(s) => s,
            None => return Vec::new(),
        };
        let tool_ids = step.assistant_tool_ids();
        step.pending_tools()
            .into_iter()
            .filter(|t| tool_ids.contains(&t.id) && !step.tool_result_has_id(&t.id))
            .collect()
    }

    /// Push a tool result directly (for manual execution).
    pub fn push_tool_result_direct(&mut self, tool_call_id: &str, result: &str, success: bool) {
        if self.push_tool_result_inner(tool_call_id, result, success, None) {
            self.context_revision = self.context_revision.saturating_add(1);
        }
    }

    /// 同 `push_tool_result_direct`，另附着展示平面 diff（编辑/写入类工具）。
    /// diff 仅存在于结构化 ToolResult（不进模型投影），供 timeline 抽屉消费。
    pub fn push_tool_result_direct_with_diff(
        &mut self,
        tool_call_id: &str,
        result: &str,
        success: bool,
        diff: Option<String>,
    ) {
        if self.push_tool_result_inner(tool_call_id, result, success, diff) {
            self.context_revision = self.context_revision.saturating_add(1);
        }
    }

    /// Execute all pending tools in the current step. When `tool_executor` is None
    /// (e.g. during session restore), returns early without injecting errors.
    pub fn execute_tools_batch(&mut self) -> Effect {
        let executor = match &self.tool_executor {
            Some(e) => e,
            None => {
                log::warn!("execute_tools_batch: no tool executor set — skipping tool execution");
                return Effect::None;
            }
        };

        let pending: Vec<PendingTool> = {
            let step = match self.turns.last().and_then(|t| t.steps.last()) {
                Some(s) => s,
                None => return Effect::None,
            };
            let tool_ids = step.assistant_tool_ids();
            step.pending_tools()
                .into_iter()
                .filter(|t| tool_ids.contains(&t.id) && !step.tool_result_has_id(&t.id))
                .collect()
        };

        if pending.is_empty() {
            return Effect::None;
        }

        let mut reports: Vec<(String, String, bool)> = Vec::new();
        for tool in &pending {
            let req = ToolExecRequest {
                id: tool.id.clone(),
                name: tool.name.clone(),
                args: tool.args.clone(),
            };
            let report = executor(req);
            reports.push((tool.id.clone(), report.content, report.success));
        }
        let mut changed = false;
        for (tc_id, content, success) in reports {
            changed |= self.push_tool_result_inner(&tc_id, &content, success, None);
        }
        if changed {
            self.context_revision = self.context_revision.saturating_add(1);
        }

        // Tools executed; caller re-evaluates (build context → gate → push_assistant)
        Effect::None
    }

    pub fn last_step_tool_results(&self) -> Vec<(String, String, String, bool, Option<String>)> {
        let step = match self.turns.last().and_then(|t| t.steps.last()) {
            Some(s) => s,
            None => return Vec::new(),
        };
        let mut results = Vec::new();
        for tr in &step.tool_results {
            if let Some((tc_id, result_text, ok, diff)) = tr.content.iter().find_map(|b| {
                if let qaqh_types::ContentBlock::ToolResult {
                    tool_use_id,
                    result,
                } = b
                {
                    Some((
                        tool_use_id.clone(),
                        result.model.text.clone(),
                        result.is_success(),
                        result.diff.clone(),
                    ))
                } else {
                    None
                }
            }) {
                let tool_name = step
                    .assistant
                    .content
                    .iter()
                    .find_map(|b| {
                        if let qaqh_types::ContentBlock::ToolUse { id, name, .. } = b {
                            if id == &tc_id {
                                Some(name.clone())
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();
                results.push((tc_id, tool_name, result_text, ok, diff));
            }
        }
        results
    }

    pub fn tool_call_args(&self, tool_call_id: &str) -> Option<serde_json::Value> {
        let step = self.turns.last().and_then(|t| t.steps.last())?;
        step.assistant.content.iter().find_map(|b| {
            if let qaqh_types::ContentBlock::ToolUse { id, input, .. } = b {
                if id == tool_call_id {
                    Some(input.clone())
                } else {
                    None
                }
            } else {
                None
            }
        })
    }

    /// Find the tool_call_id of a named tool in the most recent assistant step.
    pub fn find_last_step_tool_call(&self, tool_name: &str) -> Option<String> {
        let step = self.turns.last()?.steps.last()?;
        step.assistant.content.iter().find_map(|block| match block {
            qaqh_types::ContentBlock::ToolUse { name, id, .. } if name == tool_name => {
                Some(id.clone())
            }
            _ => None,
        })
    }

    pub fn has_pending_tools(&self) -> bool {
        self.turns
            .last()
            .and_then(|t| t.steps.last())
            .map(|s| !s.all_tools_satisfied())
            .unwrap_or(false)
    }

    pub fn turn_count(&self) -> usize {
        self.turns.len()
    }

    pub fn message_count(&self) -> usize {
        self.system_messages.len()
            + self
                .turns
                .iter()
                .map(|t| {
                    1 + t
                        .steps
                        .iter()
                        .map(|s| 1 + s.tool_results.len())
                        .sum::<usize>()
                })
                .sum::<usize>()
    }

    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }

    pub fn system_messages(&self) -> &[Message] {
        &self.system_messages
    }

    pub fn to_vec(&self) -> Vec<Message> {
        let mut v: Vec<Message> = self.system_messages.clone();
        // Write-order merge (same as gate context): full-snapshot round-trips
        // (snapshot_full → save_full → from_messages) and compact checkpoints
        // preserve the interleaved position of trailing injections instead of
        // flattening them to the tail.
        v.extend(self.flat_in_write_order(0));
        v
    }

    pub fn set_tool_executor(&mut self, executor: ToolExecutorFn) {
        self.tool_executor = Some(executor);
    }

    /// Save all messages (full rewrite). Used for undo or compact.
    /// No-op if the session seed has not been initialized yet.
    pub fn snapshot_full(&mut self, model: &str, effort: &str) {
        if self.seed.is_empty() || self.ephemeral {
            return;
        }
        let msgs = self.to_vec();
        let turn_count = self.turns.len();
        if self.has_compact_context {
            SessionManager::global().save_compact_context(&self.seed, &msgs);
        } else {
            SessionManager::global().save_full(
                &self.seed,
                &msgs,
                model,
                Some(effort),
                self.compact_skip,
                turn_count,
            );
        }
        self.pending_save.clear();
    }

    /// Reconstruct the internal turn/step structure by replaying saved messages
    /// through `push_user` / `push_assistant` / `push_tool_result`.
    pub fn from_messages(seed: &str, msgs: &[Message], compact_skip: usize) -> (Self, Vec<String>) {
        let mut store = Self::new(seed);
        store.compact_skip = compact_skip;
        store.replaying = true;
        let mut repairs = Vec::new();
        let mut i = 0;

        // Keep the base prompt plus distinct protected skill injections.
        // Other duplicate system messages from old persistence bugs are dropped.
        let mut has_system = false;
        let mut protected_system_texts = std::collections::HashSet::new();
        while i < msgs.len() && msgs[i].role == "system" {
            let text = msgs[i]
                .content
                .iter()
                .find_map(|block| match block {
                    qaqh_types::ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .unwrap_or("");
            let protected_skill =
                text.starts_with("[QAQH_SKILL_V1]") && text.contains("[END_QAQH_SKILL_V1]");
            if !has_system || (protected_skill && protected_system_texts.insert(text.to_string())) {
                store.system_messages.push(msgs[i].clone());
                has_system = true;
            } else {
                repairs.push(
                    "dropped duplicate system message (msg_id collision or prior bug)".into(),
                );
            }
            if let Some(mid) = msgs[i].msg_id {
                store.next_msg_id = store.next_msg_id.max(mid + 1);
            }
            i += 1;
        }

        while i < msgs.len() {
            match msgs[i].role.as_str() {
                "user" => {
                    let text = msgs[i]
                        .content
                        .iter()
                        .find_map(|b| {
                            if let qaqh_types::ContentBlock::Text { text } = b {
                                Some(text.clone())
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default();
                    // 子代理注入（user 角色 + [SUBAGENT 前缀）：恢复为 trailing，
                    // 不当作真实用户 turn（不建回合、不触发 timeline）。经
                    // push_trailing_system 回放以分配写入顺序 msg_id（replaying
                    // 期间 save_msg 不落盘），使 to_vec/build_context 的写入顺序
                    // 合并能恢复注入的原始时间位置。
                    if text.starts_with("[SUBAGENT ") {
                        store.push_trailing_system(msgs[i].clone());
                    } else {
                        store.push_user(&text);
                    }
                    i += 1;
                }
                "assistant" => {
                    store.push_assistant(msgs[i].clone());
                    i += 1;
                }
                "tool" => {
                    let (tc_id, result, success) = msgs[i]
                        .content
                        .iter()
                        .find_map(|b| {
                            if let qaqh_types::ContentBlock::ToolResult {
                                tool_use_id,
                                result,
                            } = b
                            {
                                Some((
                                    tool_use_id.clone(),
                                    result.model.text.clone(),
                                    result.is_success(),
                                ))
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default();
                    store.push_tool_result(&tc_id, &result, success);
                    i += 1;
                }
                _ => {
                    // Mid-stream system/developer messages: recognize persisted
                    // activation-set envelopes AND sub-agent result injections
                    // ([SUBAGENT ...] tag) and restore them as trailing
                    // injections (消息流末尾，wire 序列合法)；显式 developer
                    // role 的消息（ContextFlow 注入）无论文本一律恢复为
                    // trailing；drop anything else silently. 此前 [SUBAGENT]
                    // 被恢复为 system turn、固化在 function_call_output 之后，
                    // provider 端丢弃/挂起。
                    let text = msgs[i]
                        .content
                        .iter()
                        .find_map(|block| match block {
                            qaqh_types::ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .unwrap_or("");
                    if msgs[i].role == "developer"
                        || text.starts_with("<skill_context_envelope")
                        || text.starts_with("[SUBAGENT ")
                    {
                        // 同上：经 push_trailing_system 回放，分配写入顺序 id。
                        store.push_trailing_system(msgs[i].clone());
                    }
                    i += 1;
                }
            }
        }

        for turn in store.turns.iter_mut() {
            for step in turn.steps.iter_mut() {
                let missing_ids: Vec<(String, String)> = {
                    let tool_ids = step.assistant_tool_ids();
                    tool_ids
                        .iter()
                        .filter(|id| !step.tool_result_has_id(id))
                        .map(|id| {
                            let name = step
                                .assistant
                                .content
                                .iter()
                                .find_map(|b| {
                                    if let qaqh_types::ContentBlock::ToolUse {
                                        id: tid, name, ..
                                    } = b
                                    {
                                        if tid == id { Some(name.clone()) } else { None }
                                    } else {
                                        None
                                    }
                                })
                                .unwrap_or_default();
                            (id.clone(), name)
                        })
                        .collect()
                };
                if missing_ids.is_empty() {
                    continue;
                }
                for (id, name) in missing_ids {
                    let note = format!(
                        "[RESTORE] Tool \"{name}\" had no result when session was saved — not executed.\n[HINT] Do NOT retry."
                    );
                    step.tool_results.push(Message::tool(&id, &note, false));
                    repairs.push(format!("injected [RESTORE] for orphan tool_use {}", id));
                }
            }
        }

        // Restore next_msg_id: max(msg_id) + 1, or 1 if empty.
        let max_id = msgs.iter().filter_map(|m| m.msg_id).max().unwrap_or(0);
        store.next_msg_id = store.next_msg_id.max(max_id + 1);
        store.next_turn_seq = store.next_turn_seq.max(store.turns.len() as u64 + 1);
        store.replaying = false;

        (store, repairs)
    }

    /// Mark a restored store as being driven by a compact checkpoint.  New
    /// messages still append to the immutable archive, while the checkpoint
    /// is refreshed with the active model context.
    pub fn set_compact_context_active(&mut self, active: bool) {
        self.has_compact_context = active;
    }

    /// Keep IDs monotonic against the raw archive when restoring a compact
    /// checkpoint (the checkpoint intentionally omits archived messages).
    pub fn ensure_next_msg_id(&mut self, next: u64) {
        self.next_msg_id = self.next_msg_id.max(next);
    }

    /// Allocate a session-monotonic turn ID. Compaction may shrink the active
    /// model view, but must never make a future TurnStart reuse an archived ID.
    pub fn allocate_turn_id(&mut self) -> String {
        let seq = self.next_turn_seq;
        self.next_turn_seq = self.next_turn_seq.saturating_add(1);
        format!("t{seq}")
    }

    /// Restore the turn allocator against the immutable archive.
    pub fn ensure_next_turn_seq(&mut self, next: u64) {
        self.next_turn_seq = self.next_turn_seq.max(next.max(1));
    }

    pub fn remove_last_step_if_incomplete(&mut self) -> bool {
        if let Some(turn) = self.turns.last_mut() {
            if let Some(step) = turn.steps.last() {
                if !step.all_tools_satisfied() {
                    turn.steps.pop();
                    self.context_revision = self.context_revision.saturating_add(1);
                    return true;
                }
            }
        }
        false
    }

    /// Undo semantics: keep turns strictly before `turn_id`, drop the target
    /// turn and everything after it.
    ///
    /// `turn_id` is a session-monotonic sequence (`t{next_turn_seq}`) — it is
    /// NOT an index into `turns`. After a physical compaction
    /// ([`Self::apply_compact`]) `turns` only holds the live suffix (synthetic
    /// summary + kept turns) while `next_turn_seq` keeps counting, so the live
    /// array's first element maps to `compacted_count + 1`, recovered from the
    /// summary's `[Compacted N turns]` header (same encoding
    /// [`Self::previous_compact_summary`] parses).
    pub fn truncate_before_turn(&mut self, turn_id: &str) -> bool {
        let seq: usize = match turn_id
            .strip_prefix('t')
            .and_then(|n| n.parse::<usize>().ok())
        {
            Some(n) if n > 0 => n,
            _ => return false,
        };
        // 活跃 turns 数组起点的全局序号：无 compact 时 = 1；物理 compact 后
        // turns[0] 是合成摘要（占一个数组位），起点 = 被压缩 turn 数 + 1。
        let (first_seq, summary_offset) = match self
            .turns
            .first()
            .and_then(|t| Self::compacted_turn_count(&t.user))
        {
            Some(skip) => (skip + 1, 1),
            None => (1, 0),
        };
        if seq < first_seq {
            // 撤回已被压缩的 turn：清空全部（含摘要——它正是这些 turn 的替身）。
            self.turns.clear();
            self.context_revision = self.context_revision.saturating_add(1);
            return true;
        }
        let idx = seq - first_seq + summary_offset;
        if idx >= self.turns.len() {
            return false;
        }
        self.turns.truncate(idx);
        self.context_revision = self.context_revision.saturating_add(1);
        // After truncation, need full rewrite on next save.
        true
    }

    /// Compact: keep `keep` recent turns in LLM context, physically remove older ones.
    /// Inserts the summary as a synthetic user turn before the kept turns
    /// so that `to_vec()` serializes correctly without duplicating compacted data.
    /// Sets `compact_skip` to 0 because all turns now present are live.
    ///
    /// Trailing injections in the compacted region (msg_id < the first kept
    /// turn) are folded together with the old turns: their text has already
    /// entered the compact summary input (the summary request is built with
    /// all trailing messages), so keeping them would both duplicate the text
    /// and re-position a stale injection at the "latest message" slot. Only
    /// injections in the kept region survive, at their original positions.
    pub fn apply_compact(&mut self, summary: &str, keep: usize) {
        // 防御：确保至少保留 1 个 turn，防止 caller 传入 keep=0 导致清空全部历史。
        let keep = keep.max(1);
        let skip = self.turns.len().saturating_sub(keep);
        if skip == 0 {
            return;
        }

        // Remove old compact markers
        self.system_messages.retain(|m| !m.content.iter().any(|b| matches!(b, qaqh_types::ContentBlock::Text { text } if text.starts_with("[COMPACT"))));

        // Build compact summary as a synthetic user turn (no steps).
        // The next LLM sees only the handoff summary; the original user
        // message is intentionally omitted — it's redundant with the
        // summary and, after a manual compact, the user will send a fresh
        // message to resume work.
        let compact_text = format!("[Compacted {} turns]\n{}", skip, summary.trim(),);
        // 摘要占据被压缩的最早 turn 的写入位置：与 trailing 注入按 msg_id 合并时，
        // 摘要保持最前，其后是按写入顺序的注入与保留 turn。
        let first_compacted_id = self.turns[0].user.msg_id;
        // 首个保留 turn 的写入 id：压缩区内的 trailing 注入（id < 此值）随旧 turn
        // 折叠丢弃；保留区内的注入留在原位（A1 折叠语义）。
        let first_kept_id = self.turns[skip].user.msg_id.unwrap_or(0);
        let mut compact_turn = Turn::new(Message::user(&compact_text));
        compact_turn.user.msg_id = first_compacted_id;

        // 折叠压缩区 trailing 注入：文本已进摘要输入，随旧 turn 一起丢弃，
        // 不再"复活"到摘要之后/活跃 turn 之前（双重表示）。
        self.trailing_messages
            .retain(|m| m.msg_id.map_or(true, |id| id >= first_kept_id));

        // Physically remove compacted turns, keep only the most recent `keep`.
        let kept = self.turns.split_off(skip);
        self.turns = kept;
        // Prepend compact summary as a synthetic turn before the kept turns.
        self.turns.insert(0, compact_turn);
        // No skipping needed — compacted data is physically gone.
        self.compact_skip = 0;
        self.has_compact_context = true;
        self.context_revision = self.context_revision.saturating_add(1);
    }

    /// Get the text of any previous compaction summary (for incremental update mode).
    /// Format: `[Compacted N turns]\n{summary}` — everything after the first newline.
    /// Searches turns[0] since compact summary is stored as a synthetic turn.
    pub fn previous_compact_summary(&self) -> Option<String> {
        self.turns.first().and_then(|turn| {
            turn.user.content.iter().find_map(|b| {
                if let qaqh_types::ContentBlock::Text { text } = b {
                    if text.starts_with("[Compacted") {
                        // 首个 '\n'（ASCII）之后必为 char boundary。
                        let summary = match text.find('\n') {
                            Some(n) => text.get(n + 1..).unwrap_or("").trim(),
                            None => "",
                        };
                        if summary.len() > 20 {
                            Some(summary.to_string())
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
        })
    }

    /// Parse the `[Compacted N turns]` header of a synthetic summary turn.
    /// Used to map global turn sequence numbers onto the live `turns` array
    /// after physical compaction (see [`Self::truncate_before_turn`]).
    fn compacted_turn_count(msg: &qaqh_types::Message) -> Option<usize> {
        msg.content.iter().find_map(|b| {
            if let qaqh_types::ContentBlock::Text { text } = b {
                let rest = text.strip_prefix("[Compacted ")?;
                let n = rest.split_whitespace().next()?.parse::<usize>().ok()?;
                Some(n)
            } else {
                None
            }
        })
    }

    /// Compute context composition stats from the current message store.
    /// This reflects the actual state (post-compact), unlike the API dump which lags.
    /// Returns (chat_text_tok, thinking_tok, tool_calls_tok, tool_results_tok, tools_schema_tok, system_prompt_tok, thinking_blocks, tool_call_blocks).
    /// All token fields use `qaqh_types::count_tokens` (CJK-aware heuristic), NOT raw char length.
    #[allow(clippy::too_many_arguments)]
    pub fn compute_context_stats(
        &self,
        tool_defs: Option<&[ToolDef]>,
    ) -> (u64, u64, u64, u64, u64, u64, u64, u64) {
        let mut chat_text = 0u64;
        let mut thinking = 0u64;
        let mut tool_calls = 0u64;
        let mut tool_results = 0u64;
        let mut tools_schema = 0u64;
        let mut system_prompt = 0u64;
        let mut thinking_blocks = 0u64;
        let mut tool_call_blocks = 0u64;

        // Count tool definitions (sent as JSON schema to the LLM)
        if let Some(defs) = tool_defs {
            for td in defs {
                if let Ok(json) = serde_json::to_string(td) {
                    tools_schema += qaqh_types::count_tokens(&json) as u64;
                }
            }
        }

        for m in &self.system_messages {
            for b in &m.content {
                if let qaqh_types::ContentBlock::Text { text } = b {
                    system_prompt += qaqh_types::count_tokens(text) as u64;
                }
            }
        }
        for (i, turn) in self.turns.iter().enumerate() {
            if i < self.compact_skip {
                continue;
            }
            for m in [&turn.user] {
                for b in &m.content {
                    match b {
                        qaqh_types::ContentBlock::Text { text } => {
                            chat_text += qaqh_types::count_tokens(text) as u64;
                        }
                        qaqh_types::ContentBlock::Reasoning { reasoning } => {
                            thinking += qaqh_types::count_tokens(reasoning) as u64;
                            thinking_blocks += 1;
                        }
                        qaqh_types::ContentBlock::ToolUse { .. } => {
                            // Tool call JSON ≈ token count of serialized form
                            let json = serde_json::to_string(b).unwrap_or_default();
                            tool_calls += qaqh_types::count_tokens(&json) as u64;
                            tool_call_blocks += 1;
                        }
                        qaqh_types::ContentBlock::WebSearchCall { .. } => {
                            // Server-side search action JSON ≈ serialized form
                            let json = serde_json::to_string(b).unwrap_or_default();
                            tool_calls += qaqh_types::count_tokens(&json) as u64;
                            tool_call_blocks += 1;
                        }
                        qaqh_types::ContentBlock::ToolResult { result, .. } => {
                            tool_results += qaqh_types::count_tokens(&result.model.text) as u64;
                        }
                        qaqh_types::ContentBlock::Image { .. } => {
                            // Image token count uses the MiMo formula (roughly ~256-1024 tokens depending on resolution).
                            // Use a conservative estimate of 512 tokens per image.
                            chat_text += 512;
                        }
                        qaqh_types::ContentBlock::ResponseOutputItem { .. } => {
                            // Opaque provider replay state is already accounted
                            // for by its visible Text/Reasoning/ToolUse projection.
                        }
                    }
                }
            }
            for step in turn.steps.iter() {
                for b in &step.assistant.content {
                    match b {
                        qaqh_types::ContentBlock::Text { text } => {
                            chat_text += qaqh_types::count_tokens(text) as u64;
                        }
                        qaqh_types::ContentBlock::Reasoning { reasoning } => {
                            thinking += qaqh_types::count_tokens(reasoning) as u64;
                            thinking_blocks += 1;
                        }
                        qaqh_types::ContentBlock::ToolUse { .. } => {
                            let json = serde_json::to_string(b).unwrap_or_default();
                            tool_calls += qaqh_types::count_tokens(&json) as u64;
                            tool_call_blocks += 1;
                        }
                        _ => {}
                    }
                }
                for tr in &step.tool_results {
                    for b in &tr.content {
                        if let qaqh_types::ContentBlock::ToolResult {
                            tool_use_id: _,
                            result,
                        } = b
                        {
                            // 存储字节即最终形态：工具侧折叠已定型，直接按实际
                            // 文本统计（不再按位置模拟折叠）。
                            tool_results += qaqh_types::count_tokens(&result.model.text) as u64;
                        }
                    }
                }
            }
        }
        (
            chat_text,
            thinking,
            tool_calls,
            tool_results,
            tools_schema,
            system_prompt,
            thinking_blocks,
            tool_call_blocks,
        )
    }
}

fn auto_complete_unfulfilled(step: &mut Step, reason: &str) {
    let missing: Vec<(String, String)> = {
        let tool_ids = step.assistant_tool_ids();
        tool_ids
            .iter()
            .filter(|id| !step.tool_result_has_id(id))
            .map(|id| {
                let name = step
                    .assistant
                    .content
                    .iter()
                    .find_map(|b| {
                        if let qaqh_types::ContentBlock::ToolUse { id: tid, name, .. } = b {
                            if tid == id { Some(name.clone()) } else { None }
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();
                (id.clone(), name)
            })
            .collect()
    };
    if !missing.is_empty() {
        log::warn!(
            "auto-complete: {} unfulfilled tool(s) — {}",
            missing.len(),
            reason
        );
        for (id, name) in missing {
            step.tool_results.push(Message::tool(
                &id,
                &format!("{} Tool \"{name}\" was not executed.", reason),
                false,
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_types::ContentBlock;

    fn assistant_with_tools(tools: &[(&str, &str)]) -> Message {
        Message {
            msg_id: None,
            role: "assistant".to_string(),
            name: None,
            content: tools
                .iter()
                .map(|(id, name)| ContentBlock::ToolUse {
                    id: (*id).to_string(),
                    name: (*name).to_string(),
                    input: serde_json::json!({}),
                })
                .collect(),
        }
    }

    fn context_result(context: &[Message], id: &str) -> String {
        context
            .iter()
            .flat_map(|message| &message.content)
            .find_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    result,
                    ..
                } if tool_use_id == id => Some(result.model.text.clone()),
                _ => None,
            })
            .expect("tool result must be present in gate context")
    }

    #[test]
    fn system_input_roundtrip_preserves_mid_stream_system_message() {
        let mut store = MessageStore::new_ephemeral("test");
        store.push_user("first user turn");
        store.push_assistant(Message {
            msg_id: None,
            role: "assistant".to_string(),
            name: None,
            content: vec![ContentBlock::text("first answer")],
        });
        store.push_system_input("[SUBAGENT 'explore' COMPLETED]\n\nfinal answer here");
        store.push_user("second user turn");
        store.push_assistant(Message {
            msg_id: None,
            role: "assistant".to_string(),
            name: None,
            content: vec![ContentBlock::text("second answer")],
        });

        // to_vec: mid-stream system message keeps its role and position.
        let vec = store.to_vec();
        let roles: Vec<&str> = vec.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(
            roles,
            vec!["user", "assistant", "system", "user", "assistant"]
        );
        assert!(vec[2]
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::Text { text } if text.starts_with("[SUBAGENT 'explore' COMPLETED]"))));

        // from_messages replay restores the [SUBAGENT ...] injection as a
        // TRAILING message（wire 合法位置），而非 mid-stream turn
        // （function_call_output 之后固化的非法位置曾导致 provider 丢弃），
        // 且保持其写入顺序位置（位于两个真实 turn 之间）。
        let (restored, repairs) = MessageStore::from_messages("test", &vec, 0);
        assert!(
            repairs.is_empty(),
            "replay should not report repairs: {repairs:?}"
        );
        let restored_vec = restored.to_vec();
        let restored_roles: Vec<&str> = restored_vec.iter().map(|m| m.role.as_str()).collect();
        // to_vec 是权威快照：trailing 注入必须保持写入顺序位置（此前 to_vec
        // 把 trailing 一律钉在末尾，重启全量快照会丢失注入的原始时间位置）。
        assert_eq!(
            restored_roles,
            vec!["user", "assistant", "system", "user", "assistant"]
        );
        let trailing = restored.trailing_messages();
        assert_eq!(
            trailing.len(),
            1,
            "subagent injection must restore as trailing"
        );
        assert!(trailing[0]
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::Text { text } if text.starts_with("[SUBAGENT 'explore' COMPLETED]"))));
    }

    #[test]
    fn write_order_merge_interleaves_injection_into_history() {
        // 复现线上症状的回归测试：子代理报告在回合中途落盘后，必须进入历史
        // （位于其 ack 之前、后续用户消息之前），而不是永远钉在请求末尾被
        // 模型当作"新发的 user 消息"。
        let mut store = MessageStore::new("seed");
        store.push_user("帮我调研 X");
        store.push_assistant(Message {
            msg_id: None,
            role: "assistant".to_string(),
            name: None,
            content: vec![ContentBlock::text("好的，我来调研")],
        });

        // 子代理报告在回合中途到达 → trailing 注入（user 角色 + name=subagent）。
        let mut inject = Message::user("[SUBAGENT 'explore' COMPLETED]\n\n调研结果：X 是 Y");
        inject.name = Some("subagent".into());
        assert!(store.push_trailing_system(inject));

        // 模型对报告的确认成为同一 turn 的后续 step。
        store.push_assistant(Message {
            msg_id: None,
            role: "assistant".to_string(),
            name: None,
            content: vec![ContentBlock::text("收到报告，已纳入调研")],
        });
        // 用户随后发送新消息。
        store.push_user("继续");

        let ctx = store.build_context_for_gate(&[]);
        let texts: Vec<String> = ctx
            .iter()
            .map(|m| {
                m.content
                    .iter()
                    .find_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(
            texts,
            vec![
                "帮我调研 X",
                "好的，我来调研",
                "[SUBAGENT 'explore' COMPLETED]\n\n调研结果：X 是 Y",
                "收到报告，已纳入调研",
                "继续",
            ],
            "注入必须按写入顺序位于其 ack 与后续用户消息之前"
        );
        let report = ctx
            .iter()
            .find(|m| {
                m.content.iter().any(
                    |b| matches!(b, ContentBlock::Text { text } if text.starts_with("[SUBAGENT ")),
                )
            })
            .unwrap();
        assert_eq!(report.role, "user");
        assert_eq!(report.name.as_deref(), Some("subagent"));
        // 最后一条不是报告——模型不再把它当作最新消息。
        assert!(!texts.last().unwrap().starts_with("[SUBAGENT "));
    }

    #[test]
    fn write_order_merge_survives_full_snapshot_roundtrip() {
        let mut store = MessageStore::new("seed");
        store.push_user("第一轮");
        store.push_assistant(Message {
            msg_id: None,
            role: "assistant".to_string(),
            name: None,
            content: vec![ContentBlock::text("第一轮回答")],
        });
        let mut inject = Message::user("[SUBAGENT 'x' COMPLETED]\n\n中途报告");
        inject.name = Some("subagent".into());
        store.push_trailing_system(inject);
        store.push_user("第二轮");
        store.push_assistant(Message {
            msg_id: None,
            role: "assistant".to_string(),
            name: None,
            content: vec![ContentBlock::text("第二轮回答")],
        });

        // 全量快照（undo/compact/重启）往返后，注入的写入位置必须保持不变。
        let snapshot = store.to_vec();
        let (restored, repairs) = MessageStore::from_messages("seed", &snapshot, 0);
        assert!(
            repairs.is_empty(),
            "replay should not report repairs: {repairs:?}"
        );
        let restored_vec = restored.to_vec();
        let roles: Vec<&str> = restored_vec.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(
            roles,
            vec!["user", "assistant", "user", "user", "assistant"]
        );
        // 报告位于第一轮与第二轮之间。
        let report_idx = restored_vec
            .iter()
            .position(|m| {
                m.content.iter().any(
                    |b| matches!(b, ContentBlock::Text { text } if text.starts_with("[SUBAGENT ")),
                )
            })
            .unwrap();
        assert_eq!(report_idx, 2);
        assert_eq!(restored_vec[report_idx].name.as_deref(), Some("subagent"));
        // gate 上下文与快照同序。
        let ctx = restored.build_context_for_gate(&[]);
        assert_eq!(ctx.len(), restored_vec.len());
    }

    #[test]
    fn compact_summary_keeps_write_position_before_trailing() {
        let mut store = MessageStore::new("seed");
        store.push_user("第一轮");
        store.push_assistant(Message {
            msg_id: None,
            role: "assistant".to_string(),
            name: None,
            content: vec![ContentBlock::text("第一轮回答")],
        });
        let mut inject = Message::user("[SUBAGENT 'x' COMPLETED]\n\n报告");
        inject.name = Some("subagent".into());
        store.push_trailing_system(inject);
        store.push_user("第二轮");
        store.push_assistant(Message {
            msg_id: None,
            role: "assistant".to_string(),
            name: None,
            content: vec![ContentBlock::text("第二轮回答")],
        });

        store.apply_compact("前一轮摘要", 1);
        let ctx = store.build_context_for_gate(&[]);
        let texts: Vec<String> = ctx
            .iter()
            .map(|m| {
                m.content
                    .iter()
                    .find_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .unwrap_or_default()
            })
            .collect();
        // A1 折叠语义：注入位于被压缩的第一轮（msg_id < 首个保留 turn）——
        // 其文本已进入 compact 摘要输入，随旧 turn 一起折叠，不再出现在活跃
        // 上下文（避免"注入复活 + 双重表示"）。摘要仍占据最早 turn 位置。
        assert!(texts[0].starts_with("[Compacted 1 turns]"));
        assert!(
            !texts.iter().any(|t| t.starts_with("[SUBAGENT ")),
            "compacted-region injection must be folded"
        );
        assert_eq!(texts[1], "第二轮");
        assert_eq!(texts[2], "第二轮回答");
    }

    #[test]
    fn compact_keeps_trailing_injection_in_kept_region() {
        let mut store = MessageStore::new("seed");
        store.push_user("第一轮");
        store.push_assistant(Message {
            msg_id: None,
            role: "assistant".to_string(),
            name: None,
            content: vec![ContentBlock::text("第一轮回答")],
        });
        store.push_user("第二轮");
        store.push_assistant(Message {
            msg_id: None,
            role: "assistant".to_string(),
            name: None,
            content: vec![ContentBlock::text("第二轮回答")],
        });
        // 注入落在第二个（保留）turn 之后——属于保留区，compact 后须存活。
        let mut inject = Message::user("[SUBAGENT 'y' COMPLETED]\n\n后报告");
        inject.name = Some("subagent".into());
        store.push_trailing_system(inject);

        store.apply_compact("前一轮摘要", 1);
        let ctx = store.build_context_for_gate(&[]);
        let texts: Vec<String> = ctx
            .iter()
            .map(|m| {
                m.content
                    .iter()
                    .find_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .unwrap_or_default()
            })
            .collect();
        // 摘要在前，保留 turn 居中，保留区注入在最后——不被折叠、不挂最前。
        assert!(texts[0].starts_with("[Compacted 1 turns]"));
        assert_eq!(texts[1], "第二轮");
        assert_eq!(texts[2], "第二轮回答");
        assert_eq!(texts[3], "[SUBAGENT 'y' COMPLETED]\n\n后报告");
    }

    #[test]
    fn trailing_injection_never_splits_toolcall_from_result() {
        // ② 不变量：write-order 合并后，任何 trailing 注入都不能夹在 assistant
        // 的 tool_call 与对应 tool_result 之间（会破坏工具调用对完整性）。
        let mut store = MessageStore::new("seed");
        store.push_user("跑工具");
        store.push_assistant(assistant_with_tools(&[("exec-1", "exec")]));
        // 工具结果在注入之前已提交（lap 边界先落盘结果、后落盘注入）→ 注入
        // 的写入 id 大于该步所有结果，排序后位于结果之后，不夹在中间。
        store.push_tool_result_direct("exec-1", "EXEC_RESULT", true);
        let mut inject = Message::user("[SUBAGENT 'z' COMPLETED]\n\nmid报告");
        inject.name = Some("subagent".into());
        store.push_trailing_system(inject);

        let flat = store.flat_in_write_order(0);
        // 辅助定位：tool_call 在 assistant，tool_result 在 tool 消息；注入必须
        // 位于 tool_result 之后（同一写入序），绝不位于二者之间。
        let has_tool_result = |m: &Message| {
            m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
        };
        let has_injection = |m: &Message| {
            m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { text } if text.starts_with("[SUBAGENT ")))
        };
        let result_idx = flat.iter().position(has_tool_result).unwrap();
        let inject_idx = flat.iter().position(has_injection).unwrap();
        // 注入排在 tool_result 之后（而非夹在 assistant 与其 tool_result 之间）。
        assert!(
            inject_idx > result_idx,
            "injection must not split toolcall/result"
        );
    }

    #[test]
    fn push_system_input_starts_a_turn_that_accepts_assistant_reply() {
        let mut store = MessageStore::new_ephemeral("test");
        store.push_system_input("[SUBAGENT 'x' COMPLETED]\n\nresult");
        store.push_assistant(Message {
            msg_id: None,
            role: "assistant".to_string(),
            name: None,
            content: vec![ContentBlock::text("ack")],
        });

        let vec = store.to_vec();
        let roles: Vec<&str> = vec.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, vec!["system", "assistant"]);
    }

    #[test]
    fn current_step_projects_each_result_verbatim() {
        let mut store = MessageStore::new_ephemeral("test");
        store.push_user("inspect files");
        store.push_assistant(assistant_with_tools(&[
            ("write-1", "write"),
            ("read-1", "read"),
            ("exec-1", "exec"),
        ]));
        store.push_tool_result_direct("write-1", "WRITE_RESULT", true);
        store.push_tool_result_direct("read-1", "READ_RESULT", true);
        store.push_tool_result_direct("exec-1", "EXEC_RESULT", true);

        let context = store.build_context_for_gate(&[]);

        assert_eq!(context_result(&context, "write-1"), "WRITE_RESULT");
        assert_eq!(context_result(&context, "read-1"), "READ_RESULT");
        assert_eq!(context_result(&context, "exec-1"), "EXEC_RESULT");
    }

    #[test]
    fn display_diff_flows_to_last_step_results_but_not_model_projection() {
        // 展示平面 diff：push_tool_result_direct_with_diff 存入结构化 ToolResult
        // → last_step_tool_results 带出（timeline 消费）→ 模型投影（gate context）
        // 绝不携带 diff 正文。
        let mut store = MessageStore::new_ephemeral("test");
        store.push_user("edit a file");
        store.push_assistant(assistant_with_tools(&[("edit-1", "edit")]));
        let diff = "--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n-old\n+new\n";
        store.push_tool_result_direct_with_diff(
            "edit-1",
            "[OK] edit\n  src/a.rs: 1/1 hunk(s) applied at L2 (+1 -1)",
            true,
            Some(diff.to_string()),
        );

        let results = store.last_step_tool_results();
        assert_eq!(results.len(), 1);
        let (tc_id, _name, content, success, got_diff) = &results[0];
        assert_eq!(tc_id, "edit-1");
        assert!(content.starts_with("[OK] edit"));
        assert!(*success);
        assert_eq!(got_diff.as_deref(), Some(diff));

        // 模型投影保持精简：diff 正文缺席（展示平面单独携带）。
        let context = store.build_context_for_gate(&[]);
        let projected = context_result(&context, "edit-1");
        assert_eq!(
            projected,
            "[OK] edit\n  src/a.rs: 1/1 hunk(s) applied at L2 (+1 -1)"
        );
        assert!(
            !projected.contains("+++ b/"),
            "diff leaked to model: {projected}"
        );
    }

    #[test]
    fn completed_step_projects_each_result_verbatim() {
        let mut store = MessageStore::new_ephemeral("test");
        store.push_user("first turn");
        store.push_assistant(assistant_with_tools(&[
            ("write-1", "write"),
            ("read-1", "read"),
        ]));
        store.push_tool_result_direct("write-1", "WRITE_RESULT", true);
        store.push_tool_result_direct("read-1", "READ_RESULT", true);
        store.push_user("second turn");

        let context = store.build_context_for_gate(&[]);

        assert_eq!(context_result(&context, "write-1"), "WRITE_RESULT");
        assert_eq!(context_result(&context, "read-1"), "READ_RESULT");
    }

    #[test]
    fn historical_read_preserves_full_content() {
        let mut store = MessageStore::new_ephemeral("test");
        store.push_user("inspect file");
        store.push_assistant(assistant_with_tools(&[("read-1", "read")]));
        store.push_tool_result_direct(
            "read-1",
            &serde_json::json!({
                "status": "ok",
                "path": "src/lib.rs",
                "start_line": 40,
                "end_line": 80,
                "total_lines": 300,
                "content": "sensitive and lengthy source body"
            })
            .to_string(),
            true,
        );
        store.push_user("continue");

        let stored: serde_json::Value = serde_json::from_str(&context_result(
            &store.build_context_for_gate(&[]),
            "read-1",
        ))
        .expect("stored read remains valid JSON");
        assert_eq!(stored["path"], "src/lib.rs");
        assert_eq!(stored["start_line"], 40);
        assert!(!stored["content"].as_str().unwrap().contains("folded"));
        assert!(stored.to_string().contains("lengthy source body"));
    }

    #[test]
    fn historical_edit_result_passes_through_verbatim() {
        let mut store = MessageStore::new_ephemeral("test");
        store.push_user("make edit");
        store.push_assistant(assistant_with_tools(&[("edit-1", "edit")]));
        let body = "[OK] src/lib.rs:42 +3 -2 | edit\n\n@@ -42,2 +42,3 @@\n-full diff body";
        store.push_tool_result_direct("edit-1", body, true);

        assert_eq!(
            context_result(&store.build_context_for_gate(&[]), "edit-1"),
            body
        );
    }

    #[test]
    fn content_bearing_tool_kept_intact_on_active_step() {
        // web is content-bearing: a short result stays fully visible — the
        // model must consume it without re-fetching the URL.
        let mut store = MessageStore::new_ephemeral("test");
        store.push_user("fetch page");
        store.push_assistant(assistant_with_tools(&[("web-1", "web_fetch")]));
        store.push_tool_result_direct("web-1", "PAGE_TEXT_BODY", true);

        assert_eq!(
            context_result(&store.build_context_for_gate(&[]), "web-1"),
            "PAGE_TEXT_BODY"
        );
    }

    #[test]
    fn tool_result_never_changes_after_storage() {
        // Storage-time finalization: the SAME bytes are rendered whether the
        // step is active or historical. A later step must not change the
        // earlier result (that used to fold it, breaking the prefix cache at
        // that tool message on the very next round).
        let mut store = MessageStore::new_ephemeral("test");
        store.push_user("fetch page");
        store.push_assistant(assistant_with_tools(&[("web-1", "web_fetch")]));
        store.push_tool_result_direct("web-1", "PAGE_TEXT_BODY", true);
        let active_ctx = store.build_context_for_gate(&[]);
        let active_render = context_result(&active_ctx, "web-1");

        // New step arrives → the web result is now historical.
        store.push_assistant(assistant_with_tools(&[("edit-1", "edit")]));
        store.push_tool_result_direct("edit-1", "OK_RECEIPT", true);

        let context = store.build_context_for_gate(&[]);
        assert_eq!(
            context_result(&context, "web-1"),
            active_render,
            "historical web result must render identically to the active-step render"
        );
        // 工具结果在工具侧定型，存储字节即最终形态——无论活动/历史渲染一致。
        assert_eq!(context_result(&context, "edit-1"), "OK_RECEIPT");
    }

    #[test]
    fn long_tool_result_passes_through_at_storage() {
        // message 侧不再截断：工具侧（tool_side_fold）在回传前已定型，
        // 存储层原样保存，后续渲染逐字节一致。
        // 20K > 旧 message 侧 16K 上限，但 < qaqh-types 24K 硬顶——
        // 若 message 侧截断仍存在，此断言会失败。
        let body = "paragraph\n".repeat(2_000); // ~20K chars
        let mut store = MessageStore::new_ephemeral("test");
        store.push_user("fetch page");
        store.push_assistant(assistant_with_tools(&[("web-1", "web_fetch")]));
        store.push_tool_result_direct("web-1", &body, true);

        let first = context_result(&store.build_context_for_gate(&[]), "web-1");
        assert_eq!(first, body, "store must pass the result through verbatim");

        // Same bytes on every later render (historical position included).
        store.push_assistant(assistant_with_tools(&[("edit-1", "edit_file")]));
        store.push_tool_result_direct("edit-1", "OK_RECEIPT", true);
        let second = context_result(&store.build_context_for_gate(&[]), "web-1");
        assert_eq!(first, second, "stored result must be stable across renders");
    }

    #[test]
    fn edit_failure_result_passes_through_uncut() {
        // 失败结果（NO_MATCH 候选/详情）不折叠：模型必须看到失败原因与候选才能
        // 修正重试——折叠会剪断反馈闭环（曾导致 edit 失败时模型只能盲猜
        // 重试 → TTL 过期 → 缓存失效螺旋）。
        let mut store = MessageStore::new_ephemeral("test");
        store.push_user("edit file");
        store.push_assistant(assistant_with_tools(&[("edit-1", "edit")]));
        store.push_tool_result_direct(
            "edit-1",
            "[ERROR] NO_MATCH\n  detail: old matches nothing near expected context\n  candidates: [L12, L40]",
            true,
        );

        let projected = context_result(&store.build_context_for_gate(&[]), "edit-1");
        assert!(projected.contains("NO_MATCH"));
        assert!(
            projected.contains("candidates"),
            "failure candidates must survive: {projected}"
        );
        assert!(!projected.contains("[edit diff folded"));
    }

    #[test]
    fn edit_partial_failure_passes_through_uncut() {
        // partial 模式同样透传：失败 hunk 的详情（含候选）不能被折叠。
        let mut store = MessageStore::new_ephemeral("test");
        store.push_user("edit file");
        store.push_assistant(assistant_with_tools(&[("edit-1", "edit")]));
        store.push_tool_result_direct(
            "edit-1",
            "[PARTIAL] 1 hunk(s) applied, 1 failed — re-send ONLY the failed hunks\n  hunk 1: NO_MATCH detail with candidates",
            true,
        );

        let projected = context_result(&store.build_context_for_gate(&[]), "edit-1");
        assert!(projected.contains("NO_MATCH"));
        assert!(projected.contains("hunk 1"));
        assert!(!projected.contains("[edit diff folded"));
    }

    #[test]
    fn annotation_injected_into_first_user_message_not_last() {
        // The [Environment] annotation is pinned to the FIRST user message:
        // its position never moves, so turn-1's message renders identically
        // once turn 2 arrives (prefix cache survives the turn boundary).
        let mut store = MessageStore::new_ephemeral("test");
        store.push_user("first turn");
        store.push_assistant(assistant_with_tools(&[("edit-1", "edit_file")]));
        store.push_tool_result_direct("edit-1", "OK_RECEIPT", true);
        store.push_user("second turn");

        let context = store.build_context_for_gate(&[String::from(
            "<workspace_path>F:\\QAQ-Harness</workspace_path>",
        )]);
        let users: Vec<String> = context
            .iter()
            .filter(|m| m.role == "user")
            .map(|m| {
                m.content
                    .iter()
                    .filter_map(|b| match b {
                        qaqh_types::ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect();
        assert_eq!(users.len(), 2);
        assert!(
            users[0].starts_with("[Environment]\n<workspace_path>F:\\QAQ-Harness</workspace_path>"),
            "first user message must carry the annotation, got: {}",
            users[0]
        );
        assert_eq!(
            users[1], "second turn",
            "later user messages stay untouched"
        );
    }

    #[test]
    fn prefix_stable_across_turns_with_tool_rounds() {
        // Core regression: the entire turn-1 segment (user message, tool
        // calls, tool results, final answer) must render byte-identically in
        // turn-2 requests. This was broken by (a) annotation injection into
        // the last user message and (b) position-dependent tool folding.
        let mut store = MessageStore::new_ephemeral("test");
        store.push_user("first turn");
        store.push_assistant(assistant_with_tools(&[("web-1", "web_fetch")]));
        store.push_tool_result_direct("web-1", "WEB_BODY", true);
        store.push_assistant(assistant_with_tools(&[("exec-1", "exec")]));
        store.push_tool_result_direct("exec-1", "EXEC_OUTPUT", true);
        store.push_assistant(Message {
            msg_id: None,
            role: "assistant".into(),
            name: None,
            content: vec![qaqh_types::ContentBlock::Text {
                text: "done".into(),
            }],
        });
        let turn1_ctx = store.build_context_for_gate(&[String::from("ann")]);

        store.push_user("second turn");
        let turn2_ctx = store.build_context_for_gate(&[String::from("ann")]);

        assert!(turn2_ctx.len() > turn1_ctx.len());
        let turn1_ser = serde_json::to_string(&turn1_ctx).expect("serialize turn-1 context");
        let turn2_prefix_ser =
            serde_json::to_string(&turn2_ctx[..turn1_ctx.len()]).expect("serialize turn-2 prefix");
        assert_eq!(
            turn2_prefix_ser, turn1_ser,
            "turn-1 message segment must be byte-identical in turn 2"
        );
    }

    #[test]
    fn tool_result_stays_verbatim_even_on_active_step() {
        // write/edit 的结果在工具侧定型：活动步骤也原样渲染，不再折叠。
        let mut store = MessageStore::new_ephemeral("test");
        store.push_user("write file");
        store.push_assistant(assistant_with_tools(&[("write-1", "write")]));
        store.push_tool_result_direct("write-1", "WRITE_RESULT", true);

        assert_eq!(
            context_result(&store.build_context_for_gate(&[]), "write-1"),
            "WRITE_RESULT"
        );
    }

    #[test]
    fn skill_result_remains_complete_across_steps_and_turns() {
        let activation = format!(
            "[QAQH_SKILL_V1]\nname: large\n--- instructions ---\n{}\n[END_QAQH_SKILL_V1]",
            "instruction\n".repeat(600)
        );
        let mut store = MessageStore::new_ephemeral("test");
        store.push_user("activate it");
        store.push_assistant(assistant_with_tools(&[("skill-1", "skills")]));
        store.push_tool_result_direct("skill-1", &activation, true);

        assert_eq!(
            context_result(&store.build_context_for_gate(&[]), "skill-1"),
            activation
        );
        store.push_user("continue");
        assert_eq!(
            context_result(&store.build_context_for_gate(&[]), "skill-1"),
            activation
        );
    }

    #[test]
    fn glob_listing_remains_complete_across_steps_and_turns() {
        // glob 的产物就是文件清单：折叠成首行会让模型拿不到清单、
        // 被迫换 exec rg 绕行。与 read/skills 同理由透传
        // （工具自身 max_results 熔断 = self-limited）。
        let listing = "crates/qaqh-config/src/config.rs\ncrates/qaqh-config/src/lib.rs\ncrates/qaqh-config/src/prompt.rs\ncrates/qaqh-config/src/registry.rs\n";
        let mut store = MessageStore::new_ephemeral("test");
        store.push_user("list files");
        store.push_assistant(assistant_with_tools(&[("glob-1", "glob")]));
        store.push_tool_result_direct("glob-1", listing, true);

        assert_eq!(
            context_result(&store.build_context_for_gate(&[]), "glob-1"),
            listing
        );
        store.push_user("continue");
        assert_eq!(
            context_result(&store.build_context_for_gate(&[]), "glob-1"),
            listing
        );
    }

    #[test]
    fn skills_resource_action_remains_complete_across_steps_and_turns() {
        let resource = "reference line\n".repeat(600);
        let mut store = MessageStore::new_ephemeral("test");
        store.push_user("read its reference");
        store.push_assistant(assistant_with_tools(&[("resource-1", "skills")]));
        store.push_tool_result_direct("resource-1", &resource, true);

        assert_eq!(
            context_result(&store.build_context_for_gate(&[]), "resource-1"),
            resource
        );
        store.push_user("continue");
        assert_eq!(
            context_result(&store.build_context_for_gate(&[]), "resource-1"),
            resource
        );
    }

    #[test]
    fn restore_keeps_distinct_protected_skill_system_messages() {
        let skill = "[QAQH_SKILL_V1]\nname: review\n[END_QAQH_SKILL_V1]";
        let messages = vec![
            Message::system("base prompt"),
            Message::system(skill),
            Message::system(skill),
            Message::system("stale duplicate prompt"),
            Message::user("hello"),
        ];
        let (store, repairs) = MessageStore::from_messages("test", &messages, 0);
        assert_eq!(store.system_messages().len(), 2);
        assert!(store.system_messages().iter().any(|message| {
            message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::Text { text } if text == skill))
        }));
        assert_eq!(repairs.len(), 2);
    }

    #[test]
    fn find_last_step_tool_call_returns_correct_id() {
        let mut store = MessageStore::new_ephemeral("test");
        store.push_user("hello");

        let assistant = Message {
            msg_id: None,
            role: "assistant".into(),
            name: None,
            content: vec![
                ContentBlock::Text {
                    text: "Let me ask...".into(),
                },
                ContentBlock::ToolUse {
                    id: "call_abc".into(),
                    name: "ask_user".into(),
                    input: serde_json::json!({"question": "Which?"}),
                },
            ],
        };
        store.push_assistant(assistant);

        let id = store.find_last_step_tool_call("ask_user").unwrap();
        assert_eq!(id, "call_abc");
        assert!(store.find_last_step_tool_call("bogus").is_none());
    }

    #[test]
    fn find_last_step_tool_call_no_turns_returns_none() {
        let store = MessageStore::new_ephemeral("test");
        assert!(store.find_last_step_tool_call("ask_user").is_none());
    }

    #[test]
    fn compact_does_not_reuse_archived_turn_ids() {
        let mut store = MessageStore::new_ephemeral("test");
        for index in 1..=5 {
            assert_eq!(store.allocate_turn_id(), format!("t{index}"));
            store.push_user(&format!("turn {index}"));
        }

        store.apply_compact("checkpoint", 2);

        assert_eq!(store.turn_count(), 3); // summary + two kept turns
        assert_eq!(store.allocate_turn_id(), "t6");
    }

    #[test]
    fn truncate_before_turn_uses_global_sequence_after_compact() {
        let mut store = MessageStore::new_ephemeral("test");
        for index in 1..=5 {
            store.allocate_turn_id();
            store.push_user(&format!("turn {index}"));
        }
        // 物理裁剪：turns = [summary, t4, t5]，next_turn_seq 继续涨（t6 下一个）。
        store.apply_compact("checkpoint", 2);
        assert_eq!(store.turn_count(), 3);

        // 撤 t5（活跃最后一条）：t5 → idx 2 → 删除 t5，剩 [summary, t4]。
        assert!(store.truncate_before_turn("t5"));
        assert_eq!(store.turn_count(), 2);

        // 撤 t4（第一个活跃 turn）：保留合成摘要，剩 [summary]。
        assert!(store.truncate_before_turn("t4"));
        assert_eq!(store.turn_count(), 1);

        // 撤已被 compact 的 t3（序号早于活跃段起点）：清空（含摘要）。
        assert!(store.truncate_before_turn("t3"));
        assert_eq!(store.turn_count(), 0);

        // 越界序号（未来 turn）拒绝。
        assert!(!store.truncate_before_turn("t99"));
        // 非数字/零前缀拒绝。
        assert!(!store.truncate_before_turn("abc"));
        assert!(!store.truncate_before_turn("t0"));
    }

    #[test]
    fn truncate_before_turn_without_compact_keeps_index_equivalence() {
        let mut store = MessageStore::new_ephemeral("test");
        for index in 1..=4 {
            store.allocate_turn_id();
            store.push_user(&format!("turn {index}"));
        }
        // 无 compact：t1..t4 即 turns[0..4]，next_turn_seq = 5。
        assert!(store.truncate_before_turn("t3"));
        assert_eq!(store.turn_count(), 2); // 剩 t1, t2
        assert!(!store.truncate_before_turn("t3")); // 已删，越界拒绝
    }

    #[test]
    fn repeated_compact_replaces_the_old_summary_in_gate_context() {
        let mut store = MessageStore::new_ephemeral("test");
        for index in 1..=5 {
            store.push_user(&format!("turn {index}"));
        }
        store.apply_compact("first checkpoint body", 2);
        store.push_user("work after first checkpoint");
        store.push_user("latest work");

        store.apply_compact("second checkpoint body", 2);

        let serialized = serde_json::to_string(&store.build_context_for_gate(&[])).unwrap();
        assert!(serialized.contains("second checkpoint body"));
        assert!(!serialized.contains("first checkpoint body"));
        assert_eq!(serialized.matches("[Compacted ").count(), 1);
    }

    #[test]
    fn restored_turn_allocator_can_follow_immutable_archive_count() {
        let messages = vec![Message::user("active summary"), Message::user("recent")];
        let (mut store, _) = MessageStore::from_messages("test", &messages, 0);
        store.ensure_next_turn_seq(31);

        assert_eq!(store.allocate_turn_id(), "t31");
    }

    #[test]
    fn from_messages_restores_user_subagent_injection_as_trailing() {
        // user 角色 + name="subagent" + [SUBAGENT 前缀：注入消息回放时
        // 恢复为 trailing，不得当作真实用户 turn（不建回合）。
        let mut inject = Message::user("[SUBAGENT 'replay_test_x' COMPLETED]\n\nREPLAY-TEST-OK-X");
        inject.name = Some("subagent".into());
        let messages = vec![
            Message::system("base instructions"),
            Message::user("real user message"),
            inject,
        ];
        let (store, _) = MessageStore::from_messages("test", &messages, 0);

        // 只有一条真实用户 turn；注入在 trailing。
        assert_eq!(store.turn_count(), 1);
        assert_eq!(store.trailing_messages.len(), 1);
        let text = store.trailing_messages[0]
            .content
            .iter()
            .find_map(|b| match b {
                qaqh_types::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .unwrap_or("");
        assert!(text.contains("replay_test_x"));

        // gate context：system + 真实 user + 注入 user（trailing 在 turns 之后）。
        let ctx = store.build_context_for_gate(&[]);
        let roles: Vec<&str> = ctx.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, vec!["system", "user", "user"]);
        let last = ctx.last().unwrap();
        assert_eq!(last.name.as_deref(), Some("subagent"));
    }
}
