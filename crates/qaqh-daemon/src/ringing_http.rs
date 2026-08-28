//! Ringing HTTP command/query + 三 SSE 事件流（daemon 侧传输层，T5）。
//! 已迁移至 axum：本文件仅保留与 axum 共享的纯函数/常量及测试，
//! 手写 TCP  handlers（handle_ringing_http/handle_sse 等）已在 P4 下线。

#![allow(dead_code, unused_imports)]

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use qaqh_domain::RingingChannel;
use qaqh_ringing::{
    RINGING_BASE_PATH, RingingCommandState, RingingResetRequired,
};
use qaqh_runtime::RingingHub;

const RENEW_TTL_MS: u64 = 30_000;

fn lease_ttl_ms() -> u64 {
    std::env::var("QAQH_TEST_LEASE_TTL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(RENEW_TTL_MS)
}

const RENEW_INTERVAL_MS: u64 = 10_000;

const RINGING_TIMELINE_BASE_PATH: &str = RINGING_BASE_PATH;

const SSE_KEEPALIVE_MS: u64 = 15_000;

pub use qaqh_runtime::ringing::{PendingCommandStore, RingingLeaseStore};

fn parse_channel(s: &str) -> Option<RingingChannel> {
    match s {
        "control" => Some(RingingChannel::Control),
        "conversation" => Some(RingingChannel::Conversation),
        "tool" => Some(RingingChannel::Tool),
        _ => None,
    }
}

fn sse_frame(
    epoch: &str,
    channel: RingingChannel,
    envelope: &qaqh_ringing::RingingEventEnvelope,
) -> String {
    let event_type = serde_json::to_value(&envelope.event)
        .ok()
        .and_then(|v| v["type"].as_str().map(|s| s.to_string()))
        .unwrap_or_else(|| "message".into());
    // data 必须是**完整信封**（含 seed/stream_seq/event_id/event）：
    // 客户端按 RingingEventEnvelope 解析，缺 seed 将无法按会话路由，
    // 缺 event_id 将破坏 renderer 幂等。
    let data = serde_json::to_string(envelope).unwrap_or_else(|_| "{}".into());
    format!(
        "id: {}:{}:{}\nevent: {}\ndata: {}\n\n",
        epoch,
        channel.as_str(),
        envelope.stream_seq,
        event_type,
        data
    )
}

fn parse_query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| v.to_string())
    })
}

fn hydrate_attachment_previews(
    hub: &RingingHub,
    seed: &str,
    command: &mut qaqh_ringing::RingingCommand,
) -> Result<(), String> {
    qaqh_runtime::ringing::hydrate_attachment_previews(hub, seed, command)
}

fn parse_timeline_query(raw_path: &str) -> (Option<String>, Option<usize>) {
    let Some(query) = raw_path.split('?').nth(1) else {
        return (None, None);
    };
    let mut before_turn = None;
    let mut limit = None;
    for kv in query.split('&') {
        let Some((k, v)) = kv.split_once('=') else {
            continue;
        };
        match k {
            "before_turn" => before_turn = Some(v.to_string()),
            "limit" => limit = v.parse::<usize>().ok(),
            _ => {}
        }
    }
    (before_turn, limit)
}

fn paginate_turns(
    turns: Vec<qaqh_domain::TimelineTurn>,
    before_turn: Option<&str>,
    limit: usize,
) -> (Vec<qaqh_domain::TimelineTurn>, bool) {
    if turns.is_empty() {
        return (turns, false);
    }
    let (start, end) = match before_turn {
        Some(id) => {
            let idx = turns
                .iter()
                .position(|t| t.turn_id == id)
                .unwrap_or(turns.len());
            (idx.saturating_sub(limit), idx)
        }
        None => (turns.len().saturating_sub(limit), turns.len()),
    };
    let page: Vec<_> = turns[start..end].to_vec();
    let has_more = start > 0;
    (page, has_more)
}

fn timeline_sse_frame(epoch: &str, seed: &str, entry: &qaqh_domain::TimelineEntry) -> String {
    let data = serde_json::json!({
        "schema": "qaqh.Ringing",
        "version": 1,
        "server_epoch": epoch,
        "seed": seed,
        "entry": entry,
    });
    format!(
        "id: {epoch}:timeline:{}\nevent: timeline.entry\ndata: {}\n\n",
        entry.timeline_seq,
        serde_json::to_string(&data).unwrap_or_else(|_| "{}".into())
    )
}

fn parse_timeline_cursor(cursor: &str, epoch: &str) -> u64 {
    let mut parts = cursor.split(':');
    let received_epoch = parts.next().unwrap_or_default();
    let kind = parts.next().unwrap_or_default();
    let seq = parts.next().and_then(|value| value.parse::<u64>().ok());
    if received_epoch == epoch && kind == "timeline" && parts.next().is_none() {
        seq.unwrap_or(0)
    } else {
        0
    }
}

fn is_allowed_action(method: &str) -> bool {
    method.starts_with("git.")
        || method.starts_with("workspace.")
        || method.starts_with("config.")
        // 命名 profiles（qaqh-client ActionRequest::Profile* 已类型化暴露）。
        || method.starts_with("profile.")
        || method.starts_with("skills.")
        || method.starts_with("stats.")
        || method.starts_with("plan.")
        || method.starts_with("todo.")
        || method.starts_with("subagent.")
        || method == "session.set_tool_mode"
}

fn action_fingerprint(method: &str, params: &serde_json::Value) -> Result<String, String> {
    let payload = serde_json::to_vec(&serde_json::json!({
        "method": method,
        "params": params,
    }))
    .map_err(stringify)?;
    Ok(qaqh_runtime::ringing::content_store::sha256_hex(&payload))
}

fn session_close_seed(close_seed: &str, envelope_seed: &Option<String>) -> String {
    if !close_seed.is_empty() {
        close_seed.to_string()
    } else {
        envelope_seed.clone().unwrap_or_default()
    }
}

fn publish_session_created(hub: &RingingHub, seed: &str, command_id: &str) {
    publish_session_state(hub, seed, qaqh_domain::SessionState::Created, command_id);
}

fn publish_session_state(
    hub: &RingingHub,
    seed: &str,
    state: qaqh_domain::SessionState,
    command_id: &str,
) {
    let _ = hub.publish_with_causation(
        seed,
        qaqh_domain::DomainEvent::Control(qaqh_domain::ControlEvent::SessionStateChanged {
            seed: seed.to_string(),
            state,
        }),
        Some(command_id),
    );
}

fn sse_reset_frame(reset: &RingingResetRequired) -> String {
    let data = serde_json::to_string(reset).unwrap_or_else(|_| "{}".into());
    format!("event: ringing.reset_required\ndata: {data}\n\n")
}

fn filter_replay_for_session(
    mut replay: qaqh_runtime::ringing::hub::ChannelReplay,
    session_id: &str,
    leases: &Arc<Mutex<RingingLeaseStore>>,
) -> qaqh_runtime::ringing::hub::ChannelReplay {
    let mut leases = leases.lock().unwrap_or_else(|e| e.into_inner());
    replay
        .events
        .retain(|event| leases.owns_seed(session_id, &event.seed));
    replay
        .resets
        .retain(|reset| leases.owns_seed(session_id, &reset.seed));
    replay
}

fn parse_sse_cursor(cursor: &str, epoch: &str, channel: RingingChannel) -> u64 {
    let mut parts = cursor.split(':');
    let e = parts.next().unwrap_or("");
    let c = parts.next().unwrap_or("");
    let seq = parts
        .next()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    if e == epoch && c == channel.as_str() {
        seq
    } else {
        0
    }
}


pub(crate) fn stringify(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_lifecycle_ttl_renew_expiry() {
        let mut store = RingingLeaseStore::new();
        // open 关联双 id：client_instance_id（校验键）+ client_session_id（续租键）
        store.open("cs-1".into(), "ci-1".into());
        assert!(store.is_active("ci-1"));
        // 命令/切流端点用 client_instance_id 校验
        assert!(store.is_active("ci-1"));
        assert!(!store.is_active("unknown"));
        // renew 用 client_session_id 反查续期
        assert!(store.renew("cs-1"));
        // 过期模拟：直接改内部时间
        store.set_expiry_for_test("ci-1", Instant::now() - Duration::from_secs(1));
        assert!(!store.is_active("ci-1"));
        assert!(!store.renew("cs-1"));
    }

    #[test]
    fn action_whitelist_allows_session_set_tool_mode() {
        assert!(is_allowed_action("session.set_tool_mode"));
        assert!(is_allowed_action("config.save"));
        assert!(is_allowed_action("subagent.spawn"));
        assert!(!is_allowed_action("session.new"));
        assert!(!is_allowed_action("conversation.send_message"));
    }

    #[test]
    fn channel_parsing() {
        assert_eq!(parse_channel("control"), Some(RingingChannel::Control));
        assert_eq!(
            parse_channel("conversation"),
            Some(RingingChannel::Conversation)
        );
        assert_eq!(parse_channel("tool"), Some(RingingChannel::Tool));
        assert_eq!(parse_channel("bogus"), None);
    }

    #[test]
    fn sse_cursor_parsing() {
        assert_eq!(
            parse_sse_cursor("epoch-1:tool:42", "epoch-1", RingingChannel::Tool),
            42
        );
        assert_eq!(
            parse_sse_cursor("epoch-2:tool:42", "epoch-1", RingingChannel::Tool),
            0
        );
        assert_eq!(
            parse_sse_cursor("epoch-1:conversation:7", "epoch-1", RingingChannel::Tool),
            0
        );
        assert_eq!(
            parse_sse_cursor("garbage", "epoch-1", RingingChannel::Tool),
            0
        );
    }

    #[test]
    fn timeline_cursor_is_separate_from_ringing_v1_channel_cursors() {
        assert_eq!(parse_timeline_cursor("epoch-1:timeline:42", "epoch-1"), 42);
        assert_eq!(parse_timeline_cursor("epoch-1:tool:42", "epoch-1"), 0);
        assert_eq!(parse_timeline_cursor("epoch-2:timeline:42", "epoch-1"), 0);
        assert_eq!(
            parse_timeline_cursor("epoch-1:timeline:42:extra", "epoch-1"),
            0
        );
    }

    fn paged_turns(n: usize) -> Vec<qaqh_domain::TimelineTurn> {
        (1..=n)
            .map(|i| qaqh_domain::TimelineTurn {
                turn_id: format!("t{i}"),
                created_seq: i as u64,
                user_text: format!("q{i}"),
                sealed: true,
                state: qaqh_domain::TimelineTurnState::Completed,
                failure: None,
                rounds: vec![],
            })
            .collect()
    }

    #[test]
    fn timeline_pagination_first_page_is_tail_window() {
        let (page, has_more) = paginate_turns(paged_turns(40), None, 30);
        assert_eq!(page.len(), 30);
        assert_eq!(page.first().unwrap().turn_id, "t11");
        assert_eq!(page.last().unwrap().turn_id, "t40");
        assert!(has_more, "40 回合取尾 30 → 还有更早 10 个");
    }

    #[test]
    fn timeline_pagination_short_session_has_no_more() {
        let (page, has_more) = paginate_turns(paged_turns(10), None, 30);
        assert_eq!(page.len(), 10);
        assert!(!has_more);
    }

    #[test]
    fn timeline_pagination_before_turn_fetches_earlier_page() {
        let (page, has_more) = paginate_turns(paged_turns(40), Some("t11"), 10);
        assert_eq!(page.len(), 10);
        assert_eq!(page.first().unwrap().turn_id, "t1");
        assert_eq!(page.last().unwrap().turn_id, "t10");
        assert!(!has_more, "t11 之前只有 10 个，已到头");
    }

    #[test]
    fn timeline_pagination_before_turn_mid_page_and_unknown_fallback() {
        // t21 之前取 10 个 → t11..t20，且 t11 之前还有 → has_more。
        let (page, has_more) = paginate_turns(paged_turns(40), Some("t21"), 10);
        assert_eq!(page.first().unwrap().turn_id, "t11");
        assert_eq!(page.last().unwrap().turn_id, "t20");
        assert!(has_more);
        // 未知 before_turn 兜底取尾部页。
        let (page, _) = paginate_turns(paged_turns(40), Some("t-unknown"), 10);
        assert_eq!(page.last().unwrap().turn_id, "t40");
        // 空列表。
        let (page, has_more) = paginate_turns(vec![], Some("t1"), 10);
        assert!(page.is_empty());
        assert!(!has_more);
    }

    #[test]
    fn timeline_query_parses_before_turn_and_limit() {
        assert_eq!(
            parse_timeline_query("/ringing/v1/sessions/s1/timeline?before_turn=t11&limit=10"),
            (Some("t11".into()), Some(10))
        );
        assert_eq!(
            parse_timeline_query("/ringing/v1/sessions/s1/timeline?limit=abc"),
            (None, None)
        );
        assert_eq!(
            parse_timeline_query("/ringing/v1/sessions/s1/timeline"),
            (None, None)
        );
    }

    #[test]
    fn sse_frame_format_matches_plan() {
        let env = qaqh_ringing::RingingEventEnvelope::new(
            "s1",
            7,
            3,
            2,
            "e1",
            qaqh_ringing::RingingEvent::Tool(qaqh_domain::ToolEvent::ToolStarted {
                tool_call_id: "c".into(),
                turn_id: "t".into(),
                round_num: 0,
                name: "exec".into(),
            }),
        );
        let frame = sse_frame("epoch-1", RingingChannel::Tool, &env);
        assert!(frame.starts_with("id: epoch-1:tool:7\nevent: tool_started\ndata: "));
        assert!(frame.ends_with("\n\n"));
        // data 必须是完整信封：含 seed（renderer 按会话路由）与 event_id（幂等）
        let data = frame
            .split("\ndata: ")
            .nth(1)
            .expect("data field")
            .trim_end_matches("\n\n");
        let parsed: serde_json::Value = serde_json::from_str(data).expect("data is JSON");
        assert_eq!(parsed["seed"], "s1");
        assert_eq!(parsed["event_id"], "e1");
        assert_eq!(parsed["stream_seq"], 7);
        assert_eq!(parsed["event"]["type"], "tool_started");
    }

    #[test]
    fn sse_reset_frame_format() {
        let reset = RingingResetRequired::new(RingingChannel::Tool, "s1", 7);
        let frame = sse_reset_frame(&reset);
        assert!(frame.starts_with("event: ringing.reset_required\ndata: "));
        assert!(frame.ends_with("\n\n"));
        assert!(frame.contains("\"seed\":\"s1\""));
        assert!(frame.contains("\"earliest_available_seq\":7"));
    }

    #[test]
    fn action_fingerprint_matches_js_client_wire_bytes() {
        // Electron 客户端按 JSON 插入序 stringify；daemon 必须按 wire 字节序
        // 复算。回归保护：参数键非字典序（lang < autoCompactThreshold 为字典序，
        // 此处故意反插）时，若 serde_json 丢失 preserve_order 会重排键，
        // 导致 config.save 等 action 报 400 fingerprint mismatch。
        let body =
            br#"{"lang":"en","autoCompactThreshold":0.75,"subagentDefaultTools":["file","exec"]}"#;
        let mut params: serde_json::Value = serde_json::from_slice(body).unwrap();
        params.as_object_mut().unwrap().remove("action_id");
        params.as_object_mut().unwrap().remove("fingerprint");
        let fingerprint = action_fingerprint("config.save", &params).unwrap();
        let js_payload =
            br#"{"method":"config.save","params":{"lang":"en","autoCompactThreshold":0.75,"subagentDefaultTools":["file","exec"]}}"#;
        let js_fingerprint = qaqh_runtime::ringing::content_store::sha256_hex(js_payload);
        assert_eq!(
            fingerprint, js_fingerprint,
            "fingerprint payload must match JS JSON.stringify byte-for-byte"
        );
    }

    #[test]
    fn pending_command_idempotency() {
        let mut store = PendingCommandStore::new();
        assert!(store.record("cmd-1"), "first accept");
        assert!(!store.record("cmd-1"), "duplicate within TTL rejected");
        assert!(store.is_known("cmd-1"));
        assert!(store.record("cmd-2"), "distinct id accepted");
        // 回滚后允许重试
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
        let envelope = qaqh_ringing::RingingEventEnvelope::new(
            "seed",
            1,
            1,
            1,
            "event-1",
            qaqh_ringing::RingingEvent::Tool(qaqh_domain::ToolEvent::ToolFinished {
                tool_call_id: "call".into(),
                turn_id: "turn".into(),
                round_num: 0,
                result: qaqh_domain::ToolResult::ok("ok"),
            }),
        )
        .with_causation("cmd-1");
        store.observe_terminal_event(&envelope);
        // A very fast worker can publish before handle_command calls
        // mark_running; that late transition must not overwrite terminal.
        store.mark_running("cmd-1");
        let status = store
            .status_for_session("cmd-1", "session-a")
            .expect("status");
        assert_eq!(status.state, RingingCommandState::Succeeded);
        assert_eq!(status.terminal_event_id.as_deref(), Some("event-1"));
    }

    #[test]
    fn session_close_seed_resolution_prefers_command_seed() {
        assert_eq!(
            session_close_seed("s-command", &Some("s-envelope".into())),
            "s-command"
        );
        assert_eq!(session_close_seed("s-command", &None), "s-command");
        assert_eq!(
            session_close_seed("", &Some("s-envelope".into())),
            "s-envelope"
        );
        assert_eq!(session_close_seed("", &None), "");
    }

    #[test]
    fn session_create_event_carries_command_causation() {
        let hub = RingingHub::new("epoch-1");
        publish_session_created(&hub, "s-created", "cmd-create");
        let replay = hub.replay_channel_since(RingingChannel::Control, 0, false);
        assert_eq!(replay.events.len(), 1);
        assert_eq!(replay.events[0].seed, "s-created");
        assert_eq!(replay.events[0].causation_id.as_deref(), Some("cmd-create"));
        assert!(matches!(
            &replay.events[0].event,
            qaqh_ringing::RingingEvent::Control(qaqh_domain::ControlEvent::SessionStateChanged {
                state: qaqh_domain::SessionState::Created,
                ..
            })
        ));
    }

    #[test]
    fn parse_query_param_extracts_seed() {
        assert_eq!(parse_query_param("seed=abc", "seed"), Some("abc".into()));
        assert_eq!(
            parse_query_param("a=1&seed=xyz", "seed"),
            Some("xyz".into())
        );
        assert_eq!(parse_query_param("a=1", "seed"), None);
    }

    #[test]
    fn sse_replay_is_scoped_to_session_seed_leases() {
        let leases = Arc::new(Mutex::new(RingingLeaseStore::new()));
        leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .open("cs-1".into(), "ci-1".into());
        assert!(
            leases
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .attach_seed("cs-1", "seed-a")
        );

        let event_a = qaqh_ringing::RingingEventEnvelope::new(
            "seed-a",
            1,
            1,
            1,
            "event-a",
            qaqh_ringing::RingingEvent::Tool(qaqh_domain::ToolEvent::ToolStarted {
                tool_call_id: "call-a".into(),
                turn_id: "turn-a".into(),
                round_num: 0,
                name: "exec".into(),
            }),
        );
        let event_b = qaqh_ringing::RingingEventEnvelope::new(
            "seed-b",
            2,
            1,
            1,
            "event-b",
            qaqh_ringing::RingingEvent::Tool(qaqh_domain::ToolEvent::ToolStarted {
                tool_call_id: "call-b".into(),
                turn_id: "turn-b".into(),
                round_num: 0,
                name: "exec".into(),
            }),
        );
        let replay = qaqh_runtime::ringing::hub::ChannelReplay {
            events: vec![event_a, event_b],
            resets: vec![
                RingingResetRequired::new(RingingChannel::Tool, "seed-a", 1),
                RingingResetRequired::new(RingingChannel::Tool, "seed-b", 2),
            ],
        };
        let filtered = filter_replay_for_session(replay, "cs-1", &leases);
        assert_eq!(filtered.events.len(), 1);
        assert_eq!(filtered.events[0].seed, "seed-a");
        assert_eq!(filtered.resets.len(), 1);
        assert_eq!(filtered.resets[0].seed, "seed-a");
    }
}

