//! 会话标题生成（首 turn 完成后一次，冻结）。
//!
//! 对齐主流 AI 工具行为：首次对话后总结用户大致需求生成标题，后续不再改。
//! 时序设计（保证"立刻可见" + "质量优先"）：
//! 1. **主线程**：turn 终态（Completed）挂点立即用**首条用户消息截断**生成
//!    标题 → 写盘（`SessionManager::update_title`）→ 广播 `SessionMetaChanged`
//!    → 前端瞬间可见（零等待、零成本）；
//! 2. **后台线程**：`chat_sync` 一次小调用（≤64 tokens）做 LLM 总结，成功后
//!    **覆盖**截断版（几秒内完成）；失败/超时保持截断版（降级路径）。
//! 3. **冻结**：`meta.title.is_some()` 后不再生成（用户手动重命名未来接入）。
//!
//! subagent（ephemeral 一次性模式）跳过——不落盘、无标题语义。

use super::types::{RingContext, WriterEvent};

/// LLM 标题生成 prompt（专用小调用）。
const TITLE_SYSTEM: &str = "你是会话标题生成器。根据用户的第一条消息，用不超过 20 个字符的中文概括其需求。只输出标题本身：不要引号、不要标点、不要解释、不要换行。";

/// 截断标题上限（字符）。
const FALLBACK_MAX_CHARS: usize = 20;
/// LLM 标题清洗上限（字符）。
const LLM_MAX_CHARS: usize = 30;

/// 首 turn 完成挂点（engine `seal_timeline_terminal_round` 的 Completed 分支调用）。
///
/// 幂等：title 已存在（冻结）或 ephemeral 或无可总结的用户消息时零副作用。
pub fn maybe_generate_title(ctx: &mut RingContext) {
    // 冻结守卫：已有标题（含本次会话早前生成）不再生成。
    if ctx.agent.session.title.is_some() {
        return;
    }
    // subagent 一次性模式：不落盘、无标题语义。
    if ctx.agent.ephemeral {
        return;
    }
    let seed = ctx.agent.session.seed.clone();
    if seed.is_empty() {
        return;
    }
    // 首条用户消息（title 的语义锚点 = 用户首次表达的需求）。
    let Some(first_user) = first_user_text(&ctx.agent.msg) else {
        return;
    };
    let first_user = first_user.trim();
    if first_user.is_empty() {
        return;
    }

    // ── ① 立即：截断标题（instant 可见）──
    let fallback = truncate_title(first_user);
    qaqh_session::SessionManager::global().update_title(&seed, &fallback);
    ctx.agent.session.title = Some(fallback.clone());
    ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
        qaqh_domain::ControlEvent::SessionMetaChanged {
            seed: seed.clone(),
            title: Some(fallback.clone()),
        },
    ));

    // ── ② 异步：LLM 总结覆盖（失败/超时保持截断版）──
    let provider = build_provider(ctx);
    let event_tx = ctx.emitter.event_tx();
    let user_msg = first_user.to_string();
    let spawned = std::thread::Builder::new()
        .name("session-title".into())
        .spawn(move || {
            let text = match qaqh_gate::chat_sync(
                &provider,
                vec![
                    qaqh_types::Message::system(TITLE_SYSTEM),
                    qaqh_types::Message::user(&user_msg),
                ],
                64,
            ) {
                Ok(text) => text,
                Err(error) => {
                    log::warn!("[TITLE] LLM summary failed, keeping fallback: {error}");
                    return;
                }
            };
            let title = clean_title(&text);
            if title.is_empty() {
                return;
            }
            // 覆盖截断版（同一次生成流程，未冻结）。
            qaqh_session::SessionManager::global().update_title(&seed, &title);
            if let Some(tx) = event_tx {
                static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let seed_for_env = seed.clone();
                let env = qaqh_ringing::RingingWorkerEventEnvelope::new(
                    &seed,
                    format!("w-title-{seq}"),
                    qaqh_domain::DomainEvent::Control(
                        qaqh_domain::ControlEvent::SessionMetaChanged {
                            seed: seed_for_env,
                            title: Some(title),
                        },
                    )
                    .into(),
                );
                let _ = tx.send(WriterEvent::Ringing(env));
            }
        });
    if let Err(error) = spawned {
        log::warn!("[TITLE] spawn summary thread failed: {error}");
    }
}

/// 首条 user 消息的纯文本（取第一个 text block；无则 None）。
fn first_user_text(store: &qaqh_message::MessageStore) -> Option<String> {
    store.to_vec().into_iter().find_map(|m| {
        if m.role != "user" {
            return None;
        }
        m.content.iter().find_map(|b| match b {
            qaqh_types::ContentBlock::Text { text } if !text.trim().is_empty() => {
                Some(text.clone())
            }
            _ => None,
        })
    })
}

/// 截断降级：去 markdown 装饰 + 折叠空白 + 取前 N 字符。
fn truncate_title(raw: &str) -> String {
    let mut cleaned: String = raw
        .chars()
        .filter(|c| !matches!(c, '#' | '*' | '`' | '>' | '-' | '_' | '~'))
        .collect();
    // 折叠连续空白为单空格（含换行）。
    let mut out = String::with_capacity(cleaned.len());
    let mut prev_space = false;
    for c in cleaned.drain(..) {
        if c.is_whitespace() {
            if !prev_space && !out.is_empty() {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    let out = out.trim();
    out.chars().take(FALLBACK_MAX_CHARS).collect()
}

/// LLM 输出清洗：剥引号/首尾空白/换行 → 截断。
fn clean_title(raw: &str) -> String {
    let mut out = raw.trim().to_string();
    // 剥包裹引号（中文/英文成对）。
    let chars: Vec<char> = out.chars().collect();
    if chars.len() >= 2 {
        let (head, tail) = (chars[0], chars[chars.len() - 1]);
        if matches!(
            (head, tail),
            ('"', '"') | ('\'', '\'') | ('“', '”') | ('「', '」')
        ) {
            out = chars[1..chars.len() - 1].iter().collect();
        }
    }
    // 折叠空白（LLM 可能输出换行/多空格）。
    let mut folded = String::with_capacity(out.len());
    let mut prev_space = false;
    for c in out.chars() {
        if c.is_whitespace() {
            if !prev_space && !folded.is_empty() {
                folded.push(' ');
            }
            prev_space = true;
        } else {
            folded.push(c);
            prev_space = false;
        }
    }
    let folded = folded.trim();
    folded.chars().take(LLM_MAX_CHARS).collect()
}

/// 从 agent 配置构建 ProviderConfig（与 engine_turn/engine_compact 同构）。
fn build_provider(ctx: &RingContext) -> qaqh_gate::ProviderConfig {
    let ep = qaqh_config::registry::find_endpoint(
        &ctx.agent.config.provider_id,
        &ctx.agent.config.endpoint,
    );
    let is_responses = ep.as_ref().map(|e| e.protocol.as_str()) == Some("responses");
    if is_responses {
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
        )
        .with_stateful(ep.as_ref().map(|e| e.stateful).unwrap_or(false))
        .with_stream_usage(ep.as_ref().map(|e| e.include_stream_usage).unwrap_or(false));
        if let Some(endpoint) = ep.as_ref() {
            p.supports_reasoning_effort = endpoint.supports_reasoning_effort;
        }
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_strips_markdown_and_folds_whitespace() {
        assert_eq!(
            truncate_title("## 帮我修复权限掉L1的问题"),
            "帮我修复权限掉L1的问题"
        );
        assert_eq!(
            truncate_title("- 运行 cargo test\n- 看看结果"),
            "运行 cargo test 看看结果"
        );
        assert_eq!(
            truncate_title("这是一个超过二十个字符的非常长的用户需求描述文本内容"),
            "这是一个超过二十个字符的非常长的用户需求"
        );
    }

    #[test]
    fn clean_strips_quotes_and_truncates() {
        assert_eq!(clean_title("\"修复登录流程\""), "修复登录流程");
        assert_eq!(clean_title("“修复登录流程”"), "修复登录流程");
        assert_eq!(clean_title("修复登录流程\n\n第二行"), "修复登录流程 第二行");
        assert_eq!(clean_title("  "), "");
    }
}
