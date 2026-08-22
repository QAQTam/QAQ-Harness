//! CompactEngine: context compaction — token-split → prompt → LLM → apply.
//!
//! Two-step flow:
//! 1. `build_prompt_and_meta()` — synchronous, fast (token split + prompt build)
//! 2. Background: `chat_stream()` call in a thread (non-blocking, streaming
//!    tokens to frontend via CompactDelta events)
//! 3. `apply_result()` — synchronous, fast (apply on main thread)

use qaqh_session::SessionManager;

use super::types::*;
use crate::util;

/// Result produced by the background compact thread.
pub(crate) struct CompactMeta {
    pub compact_id: String,
    pub summary: String,
    pub kept_user_count: usize,
    pub head_user_count: usize,
    pub context_revision: u64,
    pub error: Option<String>,
}

/// Compaction prompt V2: structured handoff protocol.
///
/// The LLM must produce a decision-first summary with mandatory anchor fields
/// (file line counts, build status, complexity-labeled remaining work).
/// The Thinking Appendix is optional and only included under strict rules.
const COMPACT_PROMPT: &str = "\
You are performing a CONTEXT CHECKPOINT COMPACTION. Create a structured \
handoff summary for another LLM that will resume the task.\n\
\n\
## OUTPUT FORMAT (follow strictly)\n\
\n\
### Decision Log\n\
For each key decision made during the work:\n\
- **Decision**: {what was chosen}\n\
- **Alternatives**: {what was rejected and why, if any}\n\
- **Status**: done / started but not finished / cancelled\n\
\n\
### State Snapshot\n\
- **Key files** (path + approximate line count): file.rs(~L123), ...\n\
- **Build status**: cargo check output summary (errors, warnings)\n\
- **Last successful action**: {command or tool} at {timestamp or turn}\n\
\n\
### Remaining Work\n\
Use complexity labels: [small], [medium], [large]. Include rough estimates.\n\
- [small] {task}  — ~1 edit or trivial fix\n\
- [medium] {task} — multiple edits, one file\n\
- [large] {task}  — new file or cross-crate changes\n\
\n\
### Thinking Appendix (OPTIONAL — include ONLY when applicable)\n\
Include ONLY when:\n\
- ≥2 dead-end investigation paths were tried before finding the root cause\n\
  → briefly note each dead-end and why it was wrong\n\
- A cross-crate or cross-module causality chain was needed to diagnose an issue\n\
  → note the chain (e.g. \"qaqh-workspace → qaqh-message → qaqh-session\")\n\
\n\
If neither condition is met, OMIT this section entirely.\n\
\n\
## RULES\n\
- Be concise: minimum tokens with maximum information density.\n\
- Do NOT mention the compaction process itself.\n\
- Decision Log, State Snapshot, and Remaining Work are MANDATORY.\n\
- Thinking Appendix is OPTIONAL — omit if no dead-ends or cross-crate chains.\n\
- If a section has no content, write \"None\" instead of omitting it.";

/// Prefix injected before a previous summary in UPDATE MODE.
/// Tells the LLM to merge new context with the prior structured handoff,
/// preserving existing Decision Log entries unless superseded.
const SUMMARY_PREFIX: &str = "\
Another language model previously worked on this task and produced a \
structured handoff summary (below). Merge the new context with the \
previous summary to create an updated checkpoint:\n\
- Preserve existing Decision Log entries unless the new context shows \
  they were completed or superseded.\n\
- Update State Snapshot and Remaining Work with the latest information.\n\
- If the previous summary has a Thinking Appendix, decide if the new \
  context adds more dead-end paths worth preserving; otherwise drop it.";

pub struct CompactEngine;

impl CompactEngine {
    pub fn new() -> Self {
        Self
    }
    pub fn reset(&mut self) {}

    /// Step 1: Token-split, serialize, build prompt — fast, synchronous.
    /// Returns (prompt, kept_user_count, head_user_count, provider, compact_id)
    /// needed for the LLM call and apply step. Returns None if no compaction
    /// needed. compact_id 在同一函数内生成，供 CompactStarted/Finished 关联。
    pub(crate) fn build_prompt_and_meta(
        &self,
        ctx: &mut RingContext,
    ) -> Option<(String, usize, usize, qaqh_gate::ProviderConfig, String)> {
        const KEEP_TOKENS: usize = 4_000;
        let turns_total = ctx.agent.msg.turn_count();
        log::info!("[COMPACT] {} turns", turns_total);

        let all = ctx.agent.msg.build_context_for_gate(&[]);
        let msgs: Vec<&qaqh_types::Message> = all.iter().filter(|m| m.role != "system").collect();
        if msgs.is_empty() {
            return None;
        }

        let mut kept_idx = msgs.len();
        let mut kept_tokens = 0usize;
        for (i, m) in msgs.iter().enumerate().rev() {
            let t = estimate_message_tokens(m);
            if kept_tokens + t > KEEP_TOKENS {
                kept_idx = i + 1;
                break;
            }
            kept_tokens += t;
            kept_idx = i;
        }

        // 保护：当尾部消息本身超过 KEEP_TOKENS 时（如超大 tool result），
        // 标记扫描会导致 kept 区间为空 → apply_compact 清空全部 turn。
        // 回退到至少保留最后一条 user 消息所在的 turn。
        if kept_idx == msgs.len() {
            for (i, m) in msgs.iter().enumerate().rev() {
                if m.role == "user" {
                    kept_idx = i;
                    break;
                }
            }
            // 如果连一条 user 消息都没有（极端情况），放弃本次 compact。
            if kept_idx == msgs.len() {
                return None;
            }
        }

        let head_msgs = &msgs[..kept_idx];
        if head_msgs.is_empty() {
            // Ringing 双发：ToolNotice（工具域通知留在 Tool 频道）
            ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Tool(
                qaqh_domain::ToolEvent::ToolNotice {
                    tool_call_id: None,
                    level: qaqh_domain::NoticeLevel::Info,
                    message: "Compact skipped: all within token budget".into(),
                },
            ));
            return None;
        }

        let previous_summary = ctx.agent.msg.previous_compact_summary();
        let head_user_count = compactable_head_user_count(head_msgs);
        // Immediately after a successful compact, the only item before the
        // 4K tail can be the synthetic checkpoint itself. Re-compacting that
        // checkpoint removes no real turn and can repeat every gate lap when
        // fixed system/tool overhead still keeps the prompt above threshold.
        if head_user_count == 0 {
            log::debug!("[COMPACT] skipped: no new turns are eligible");
            return None;
        }
        let kept_user_count = msgs[kept_idx..].iter().filter(|m| m.role == "user").count();

        let compact_id = format!(
            "compact-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        );
        // Ringing 双发：CompactStarted（权威开始事件，携带 compact_id）
        ctx.emitter
            .emit_domain(qaqh_domain::DomainEvent::Conversation(
                qaqh_domain::ConversationEvent::CompactStarted {
                    compact_id: compact_id.clone(),
                    turns_total: turns_total as u32,
                    turns_keeping: kept_user_count as u32,
                },
            ));

        // The previous checkpoint already appears in <previous-summary> below.
        // Do not serialize the synthetic `[Compacted ...]` turn into HISTORY
        // again, or repeated compaction duplicates and recursively amplifies it.
        let history_head = compact_history_head(head_msgs, previous_summary.is_some());
        let contexts = serialize_messages(&history_head, &msgs[kept_idx..]);
        let timeline = {
            let created = ctx.agent.session.created_at;
            let updated = ctx
                .agent
                .session
                .updated_at
                .max(SessionManager::now_epoch());
            let start_str = util::epoch_to_date(created);
            let dur = updated.saturating_sub(created);
            format!(
                "- Session started: {start_str} (UTC)\n- Session duration: {}h {}m real-time",
                dur / 3600,
                (dur % 3600) / 60
            )
        };

        let prompt = if let Some(ref prev) = previous_summary {
            format!(
                "[COMPACT — UPDATE MODE]\n\n\
                 {SUMMARY_PREFIX}\n\n\
                 <previous-summary>\n{prev}\n</previous-summary>\n\n\
                 --- HISTORY (newer context to merge) ---\n\
                 {}\n\
                 --- END HISTORY ---\n\n\
                 {COMPACT_PROMPT}",
                contexts.join("\n\n"),
            )
        } else {
            format!(
                "[COMPACT]\n\n\
                 Create a new checklist summary from the conversation history.\n\n\
                 --- HISTORY ---\n\
                 {}\n\
                 --- END HISTORY ---\n\n\
                 Timeline:\n{timeline}\n\n\
                 {COMPACT_PROMPT}",
                contexts.join("\n\n"),
            )
        };

        let ep = qaqh_config::registry::find_endpoint(
            &ctx.agent.config.provider_id,
            &ctx.agent.config.endpoint,
        );
        let is_responses = ep.as_ref().map(|e| e.protocol.as_str()) == Some("responses");
        let provider = if is_responses {
            let mut p = qaqh_gate::ProviderConfig::responses(
                &ctx.agent.config.base_url,
                &ctx.agent.config.api_key,
                &ctx.agent.config.model,
                ep.as_ref().and_then(|e| e.responses_path.clone()),
            );
            if let Some(endpoint) = ep.as_ref() {
                p.responses_compat = qaqh_gate::ResponsesCompat {
                    web_search: endpoint.responses_web_search,
                    echo_web_search_call: endpoint.responses_echo_web_search_call,
                    send_include: endpoint.responses_send_include,
                    effort_max: endpoint.responses_effort_max.clone(),
                    supports_user: endpoint.responses_supports_user,
                    search_function_alias: endpoint.responses_search_function_alias.clone(),
                    echo_reasoning_content: endpoint.responses_echo_reasoning_content,
                };
            }
            p
        } else {
            let mut p = qaqh_gate::ProviderConfig::openai(
                &ctx.agent.config.base_url,
                &ctx.agent.config.api_key,
                &ctx.agent.config.model,
                ep.as_ref().and_then(|e| e.user_id_mode.clone()),
                ep.as_ref().and_then(|e| e.chat_path.clone()),
                ep.as_ref()
                    .map(|e| e.thinking_mode.clone())
                    .unwrap_or_default(),
                ep.as_ref()
                    .map(|e| e.cache_field.clone())
                    .unwrap_or_default(),
                ep.as_ref().map(|e| e.supports_thinking).unwrap_or(false),
                ep.as_ref().and_then(|e| e.do_sample),
            );
            if let Some(endpoint) = ep.as_ref() {
                p.supports_reasoning_effort = endpoint.supports_reasoning_effort;
                p.tool_call_content_null = endpoint.tool_call_content_null;
                p.supports_reasoning_content = endpoint.supports_reasoning_content;
                p.require_provider_parameters = endpoint.require_provider_parameters;
            }
            p
        };
        Some((
            prompt,
            kept_user_count,
            head_user_count,
            provider,
            compact_id,
        ))
    }

    /// Step 2: Apply compact result on the live message store (called from main thread).
    pub(crate) fn apply_result(&self, ctx: &mut RingContext, meta: &CompactMeta) {
        if meta.context_revision != ctx.agent.msg.context_revision() {
            log::warn!(
                "[COMPACT] rejecting stale result {}: source revision {}, current {}",
                meta.compact_id,
                meta.context_revision,
                ctx.agent.msg.context_revision()
            );
            ctx.emitter
                .emit_domain(qaqh_domain::DomainEvent::Conversation(
                    qaqh_domain::ConversationEvent::CompactFinished {
                        compact_id: meta.compact_id.clone(),
                        status: qaqh_domain::CompactStatus::Cancelled,
                        summary_chars: Some(0),
                        turns_compacted: Some(0),
                        turns_removed: Some(0),
                    },
                ));
            return;
        }
        if let Some(ref err) = meta.error {
            // Ringing 双发：OperationFailed（compact 失败）
            ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
                qaqh_domain::ControlEvent::OperationFailed {
                    occurrence_id: format!(
                        "occ-compact-{}",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis())
                            .unwrap_or(0),
                    ),
                    scope: qaqh_domain::ErrorScope::Conversation,
                    error: qaqh_domain::DomainError {
                        error_id: format!(
                            "err-compact-{}",
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_millis())
                                .unwrap_or(0),
                        ),
                        code: "compact_failed".into(),
                        message: err.clone(),
                        retryable: true,
                        dedupe_key: Some("compact_failed".into()),
                    },
                    operation_id: None,
                },
            ));
            // Ringing 双发：CompactFinished（失败终态）
            ctx.emitter
                .emit_domain(qaqh_domain::DomainEvent::Conversation(
                    qaqh_domain::ConversationEvent::CompactFinished {
                        compact_id: meta.compact_id.clone(),
                        status: qaqh_domain::CompactStatus::Failed,
                        summary_chars: Some(0),
                        turns_compacted: Some(0),
                        turns_removed: Some(0),
                    },
                ));
            return;
        }
        let chars = meta.summary.chars().count();

        // Turns to remove from frontend state (= total_turns - kept).
        let turns_removed = ctx
            .agent
            .msg
            .turns()
            .len()
            .saturating_sub(meta.kept_user_count);
        ctx.agent
            .msg
            .apply_compact(&meta.summary, meta.kept_user_count);
        ctx.agent
            .msg
            .snapshot_full(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);

        let (
            chat_text,
            thinking,
            tool_calls,
            tool_results,
            tools_schema,
            system_prompt,
            thinking_blocks,
            tool_call_blocks,
        ) = ctx
            .agent
            .msg
            .compute_context_stats(Some(&ctx.agent.tool_defs));
        let stats = serde_json::json!({
            "messages": ctx.agent.msg.turn_count(),
            "chat_text": chat_text, "thinking": thinking,
            "tool_calls": tool_calls, "tool_results": tool_results,
            "tools_schema": tools_schema, "system_prompt": system_prompt,
            "thinking_blocks": thinking_blocks, "tool_call_blocks": tool_call_blocks,
        });
        // 统一数据源：上下文统计并入 meta.json（原 context_stats.json 退役）。
        qaqh_session::SessionManager::global().set_context_stats(&ctx.agent.session.seed, &stats);

        // Ringing 双发：CompactFinished（成功终态）
        ctx.emitter
            .emit_domain(qaqh_domain::DomainEvent::Conversation(
                qaqh_domain::ConversationEvent::CompactFinished {
                    compact_id: meta.compact_id.clone(),
                    status: qaqh_domain::CompactStatus::Completed,
                    summary_chars: Some(chars),
                    turns_compacted: Some(meta.head_user_count as u32),
                    turns_removed: Some(turns_removed as u32),
                },
            ));
        // Ringing 双发：ToolNotice（工具域通知留在 Tool 频道）
        ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Tool(
            qaqh_domain::ToolEvent::ToolNotice {
                tool_call_id: None,
                level: qaqh_domain::NoticeLevel::Info,
                message: format!(
                    "Compacted {} turns -> {chars} chars, keeping {} turns",
                    meta.head_user_count, meta.kept_user_count,
                ),
            },
        ));
    }
}

fn estimate_message_tokens(message: &qaqh_types::Message) -> usize {
    let serialized = serde_json::to_string(message).unwrap_or_default();
    qaqh_types::count_tokens(&serialized) as usize
}

// ═══════════════════════════════════════════════════════
// Background worker — runs in a separate thread
// ═══════════════════════════════════════════════════════

/// Run the LLM compaction call in a background thread.
/// Uses streaming so the user can see the model output in real-time
/// via `CompactDelta` events pushed through `event_tx`.
/// Returns CompactMeta via the channel.
pub(crate) fn run_compact_worker(
    seed: String,
    compact_id: String,
    prompt: String,
    provider: qaqh_gate::ProviderConfig,
    kept_user_count: usize,
    head_user_count: usize,
    context_revision: u64,
    event_tx: std::sync::mpsc::SyncSender<crate::ringing_v1::types::WriterEvent>,
    causation_id: Option<String>,
) -> CompactMeta {
    let msgs_vec = vec![qaqh_types::Message::user(&prompt)];
    let mut summary = String::new();
    let mut progress_seq = 0u64;

    let mut on_event = |ev: qaqh_gate::StreamEvent| match ev {
        qaqh_gate::StreamEvent::ContentDelta(delta) => {
            summary.push_str(&delta);
            // Ringing 双发：CompactProgress（replaceable 流式摘要）
            progress_seq += 1;
            let env = qaqh_ringing::RingingWorkerEventEnvelope::new(
                seed.as_str(),
                format!("w-compact-{compact_id}-{progress_seq}"),
                qaqh_domain::DomainEvent::Conversation(
                    qaqh_domain::ConversationEvent::CompactProgress {
                        compact_id: compact_id.clone(),
                        delta,
                    },
                )
                .into(),
            );
            let env = match causation_id.as_deref() {
                Some(command_id) => env.with_causation(command_id),
                None => env,
            };
            let _ = event_tx.send(crate::ringing_v1::types::WriterEvent::Ringing(env));
        }
        qaqh_gate::StreamEvent::ReasoningDelta(delta) => {
            // legacy CompactDelta reasoning 透传已退役：Ringing 无 reasoning 专用事件，
            // 压缩过程的思考链由 CompactProgress（摘要流）覆盖（convergence-plan §4.2 登记）。
            let _ = delta;
        }
        _ => {}
    };

    match qaqh_gate::chat_stream(
        &provider,
        msgs_vec,
        None,
        20480,
        None,
        None,
        None,
        &mut on_event,
    ) {
        Ok(()) if !summary.trim().is_empty() => CompactMeta {
            compact_id,
            summary,
            kept_user_count,
            head_user_count,
            context_revision,
            error: None,
        },
        Ok(()) => CompactMeta {
            compact_id,
            summary: String::new(),
            kept_user_count,
            head_user_count,
            context_revision,
            error: Some("Compact failed: model returned empty response.".into()),
        },
        Err(e) => CompactMeta {
            compact_id,
            summary: String::new(),
            kept_user_count,
            head_user_count,
            context_revision,
            error: Some(format!("{e}")),
        },
    }
}

// ═══════════════════════════════════════════════════════
// Message serialization helpers
// ═══════════════════════════════════════════════════════

fn is_compact_summary_message(message: &qaqh_types::Message) -> bool {
    message.role == "user"
        && message.content.iter().any(|block| {
            matches!(
                block,
                qaqh_types::ContentBlock::Text { text } if text.starts_with("[Compacted ")
            )
        })
}

fn compact_history_head<'a>(
    head: &[&'a qaqh_types::Message],
    has_previous_summary: bool,
) -> Vec<&'a qaqh_types::Message> {
    if has_previous_summary {
        head.iter()
            .copied()
            .filter(|message| !is_compact_summary_message(message))
            .collect()
    } else {
        head.to_vec()
    }
}

fn compactable_head_user_count(head: &[&qaqh_types::Message]) -> usize {
    head.iter()
        .filter(|message| message.role == "user" && !is_compact_summary_message(message))
        .count()
}

fn serialize_messages(head: &[&qaqh_types::Message], kept: &[&qaqh_types::Message]) -> Vec<String> {
    let mut out = Vec::new();
    for m in head {
        let role = &m.role;
        let lines: Vec<String> = m
            .content
            .iter()
            .filter_map(|b| match b {
                qaqh_types::ContentBlock::Text { text } => Some(format!("[{role}]: {text}")),
                qaqh_types::ContentBlock::Reasoning { .. } => None,
                qaqh_types::ContentBlock::ToolUse { name, input, .. } => {
                    let args = serde_json::to_string(input).unwrap_or_default();
                    let end = args.floor_char_boundary(args.len().min(120));
                    Some(format!(
                        "[{role} tool call]: {}({})",
                        name,
                        args.get(..end).unwrap_or(&args)
                    ))
                }
                qaqh_types::ContentBlock::WebSearchCall { action, .. } => {
                    let action_str = serde_json::to_string(action).unwrap_or_default();
                    Some(format!("[{role} web search]: {action_str}"))
                }
                qaqh_types::ContentBlock::ToolResult { result, .. } => {
                    let compact: String = result
                        .model
                        .text
                        .lines()
                        .take(5)
                        .map(|l| l.chars().take(200).collect::<String>())
                        .collect::<Vec<_>>()
                        .join(" | ");
                    let end = compact.floor_char_boundary(compact.len().min(600));
                    Some(format!(
                        "[Tool result]: {}",
                        compact.get(..end).unwrap_or(&compact)
                    ))
                }
                qaqh_types::ContentBlock::Image { .. } => {
                    Some(format!("[{role}]: [Image attached]"))
                }
                qaqh_types::ContentBlock::ResponseOutputItem { .. } => None,
            })
            .collect();
        if !lines.is_empty() {
            out.push(lines.join("\n"));
        }
    }
    for m in kept {
        if m.role == "tool" {
            if let Some(qaqh_types::ContentBlock::ToolResult { result, .. }) = m.content.first() {
                let compact: String = result
                    .model
                    .text
                    .lines()
                    .take(3)
                    .map(|l| l.chars().take(200).collect::<String>())
                    .collect::<Vec<_>>()
                    .join(" | ");
                let end = compact.floor_char_boundary(compact.len().min(400));
                out.push(format!(
                    "[Tool result (recent)]: {}",
                    compact.get(..end).unwrap_or(&compact)
                ));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        compact_history_head, compactable_head_user_count, estimate_message_tokens,
        serialize_messages,
    };

    #[test]
    fn update_mode_does_not_repeat_previous_summary_in_history() {
        let old = qaqh_types::Message::user("[Compacted 4 turns]\nold checkpoint body");
        let newer = qaqh_types::Message::user("new work after checkpoint");
        let head = vec![&old, &newer];

        let filtered = compact_history_head(&head, true);
        let history = serialize_messages(&filtered, &[]).join("\n");

        assert!(!history.contains("old checkpoint body"));
        assert!(history.contains("new work after checkpoint"));
    }

    #[test]
    fn initial_mode_keeps_ordinary_history_unchanged() {
        let message = qaqh_types::Message::user("ordinary history");
        let head = vec![&message];

        let filtered = compact_history_head(&head, false);
        let history = serialize_messages(&filtered, &[]).join("\n");

        assert!(history.contains("ordinary history"));
    }

    #[test]
    fn previous_summary_alone_is_not_a_new_compaction_candidate() {
        let old = qaqh_types::Message::user("[Compacted 4 turns]\nold checkpoint body");
        let head = vec![&old];

        assert_eq!(compactable_head_user_count(&head), 0);
    }

    #[test]
    fn real_turn_after_previous_summary_is_compactable() {
        let old = qaqh_types::Message::user("[Compacted 4 turns]\nold checkpoint body");
        let real = qaqh_types::Message::user("new completed work");
        let head = vec![&old, &real];

        assert_eq!(compactable_head_user_count(&head), 1);
    }

    #[test]
    fn tail_budget_uses_tokenizer_for_cjk_content() {
        let message = qaqh_types::Message::user(&"上下文压缩".repeat(100));

        assert!(estimate_message_tokens(&message) > 300);
    }
}
