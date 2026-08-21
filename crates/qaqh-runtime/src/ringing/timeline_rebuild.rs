//! Timeline 投影重建（BUG-006）。
//!
//! timeline 是前端 transcript 的权威读侧，但 `messages.jsonl` /
//! `compact-context.json` 才是消息写侧事实。timeline 文件缺失或损坏时，
//! 这里按 conversation snapshot 同一基线（compact 优先）重建一条全 sealed
//! 的 timeline，使 timeline 恢复“可重建投影”的地位。

use qaqh_domain::{
    TimelineBlockKind, TimelineFailure, TimelineIntent, TimelineSnapshot, TimelineTool,
    TimelineToolState, TimelineTurnState,
};
use qaqh_session::SessionManager;

/// 从持久化 session 消息重建 timeline 快照 + replay journal。
pub fn rebuild_timeline_snapshot(
    seed: &str,
) -> Option<(TimelineSnapshot, Vec<qaqh_domain::TimelineEntry>)> {
    let manager = SessionManager::try_global()?;
    let (_, archive_messages, compact_context) = manager.load_for_resume(seed)?;
    let messages = compact_context
        .as_ref()
        .map(|context| context.messages.as_slice())
        .unwrap_or(archive_messages.as_slice());
    let (_, turns) = qaqh_msgloop::util::project_turns_from_messages(seed, messages, None, None);
    timeline_snapshot_from_turns(seed, &turns)
}

/// 把已经投影好的 `TurnData` 序列重放进 [`TimelineAppender`]，得到与原生
/// 写入路径同构的 snapshot + replay journal。
///
/// 重放使用 `BlockCheckpoint` 写入整段文本（不做逐 token delta），因此重建
/// 结果的每个 block 都是最终全文且 sealed；turn 终态统一标记为 `Completed`
/// （messages.jsonl 不持久化 turn 终态，这是恢复时的可接受降级）。
pub fn timeline_snapshot_from_turns(
    seed: &str,
    turns: &[qaqh_proto::TurnData],
) -> Option<(TimelineSnapshot, Vec<qaqh_domain::TimelineEntry>)> {
    if turns.is_empty() {
        return None;
    }

    let mut appender = crate::timeline::TimelineAppender::new();
    for (turn_index, turn) in turns.iter().enumerate() {
        let turn_id = if turn.turn_id.is_empty() {
            format!("t{}", turn_index + 1)
        } else {
            turn.turn_id.clone()
        };
        if let Err(error) = appender.apply_intent(
            seed,
            TimelineIntent::TurnOpened {
                turn_id: turn_id.clone(),
                user_text: turn.user_text.clone(),
            },
        ) {
            log::warn!("[timeline-rebuild] open turn {turn_id} failed: {error}");
            return None;
        }

        for (round_index, round) in turn.rounds.iter().enumerate() {
            let mut stream_segment = 0u32;
            let mut round_has_blocks = false;

            for block in &round.blocks {
                match block {
                    qaqh_proto::RoundBlock::Reasoning { content } => {
                        if content.is_empty() {
                            continue;
                        }
                        let block_id =
                            format!("round-{}:reasoning:{stream_segment}", round.round_num);
                        stream_segment = stream_segment.saturating_add(1);
                        rebuild_text_block(
                            &mut appender,
                            seed,
                            &turn_id,
                            round.round_num,
                            &block_id,
                            TimelineBlockKind::Reasoning,
                            content,
                        )?;
                        round_has_blocks = true;
                    }
                    qaqh_proto::RoundBlock::Text { content } => {
                        if content.is_empty() {
                            continue;
                        }
                        let block_id = format!("round-{}:text:{stream_segment}", round.round_num);
                        stream_segment = stream_segment.saturating_add(1);
                        rebuild_text_block(
                            &mut appender,
                            seed,
                            &turn_id,
                            round.round_num,
                            &block_id,
                            TimelineBlockKind::Text,
                            content,
                        )?;
                        round_has_blocks = true;
                    }
                    qaqh_proto::RoundBlock::Tool { card } => {
                        let block_id = format!("tool:{}", card.id);
                        let tool = rebuild_tool(card, &round.tool_results);
                        if let Err(error) = appender.apply_intent(
                            seed,
                            TimelineIntent::BlockOpened {
                                turn_id: turn_id.clone(),
                                round_num: round.round_num,
                                block_id: block_id.clone(),
                                kind: TimelineBlockKind::Tool,
                                tool: Some(tool.clone()),
                            },
                        ) {
                            log::warn!(
                                "[timeline-rebuild] open tool block {block_id} failed: {error}"
                            );
                            return None;
                        }
                        if let Err(error) = appender.apply_intent(
                            seed,
                            TimelineIntent::ToolUpdated {
                                turn_id: turn_id.clone(),
                                round_num: round.round_num,
                                block_id: block_id.clone(),
                                tool,
                            },
                        ) {
                            log::warn!(
                                "[timeline-rebuild] update tool block {block_id} failed: {error}"
                            );
                            return None;
                        }
                        if let Err(error) = appender.apply_intent(
                            seed,
                            TimelineIntent::BlockSealed {
                                turn_id: turn_id.clone(),
                                round_num: round.round_num,
                                block_id,
                            },
                        ) {
                            log::warn!("[timeline-rebuild] seal tool block failed: {error}");
                            return None;
                        }
                        round_has_blocks = true;
                    }
                    // Responses API 的内置 web search 没有原生 timeline block；
                    // 重建为文本记录行，保证它不会破坏后续 round 的轮次序号。
                    qaqh_proto::RoundBlock::WebSearch { action } => {
                        let content = serde_json::to_string(action)
                            .map(|action| format!("web_search: {action}"))
                            .unwrap_or_else(|_| "web_search".to_string());
                        let block_id = format!("round-{}:text:{stream_segment}", round.round_num);
                        stream_segment = stream_segment.saturating_add(1);
                        rebuild_text_block(
                            &mut appender,
                            seed,
                            &turn_id,
                            round.round_num,
                            &block_id,
                            TimelineBlockKind::Text,
                            &content,
                        )?;
                        round_has_blocks = true;
                    }
                }
            }

            // 旧投影可能没有 `blocks` 但有 thinking/answer 字段：兜底补两个
            // 文本块，保证老会话至少有可见内容。
            if !round_has_blocks {
                if let Some(thinking) = round.thinking.as_deref().filter(|text| !text.is_empty()) {
                    let block_id = format!("round-{}:reasoning:{stream_segment}", round.round_num);
                    stream_segment = stream_segment.saturating_add(1);
                    rebuild_text_block(
                        &mut appender,
                        seed,
                        &turn_id,
                        round.round_num,
                        &block_id,
                        TimelineBlockKind::Reasoning,
                        thinking,
                    )?;
                    round_has_blocks = true;
                }
                if let Some(answer) = round.answer.as_deref().filter(|text| !text.is_empty()) {
                    let block_id = format!("round-{}:text:{stream_segment}", round.round_num);
                    rebuild_text_block(
                        &mut appender,
                        seed,
                        &turn_id,
                        round.round_num,
                        &block_id,
                        TimelineBlockKind::Text,
                        answer,
                    )?;
                    round_has_blocks = true;
                }
            }

            if round_has_blocks {
                let is_final = round.is_final || round_index + 1 == turn.rounds.len();
                if let Err(error) = appender.apply_intent(
                    seed,
                    TimelineIntent::RoundSealed {
                        turn_id: turn_id.clone(),
                        round_num: round.round_num,
                        is_final,
                    },
                ) {
                    log::warn!("[timeline-rebuild] seal round failed: {error}");
                    return None;
                }
            }
        }

        if let Err(error) = appender.apply_intent(
            seed,
            TimelineIntent::TurnSealed {
                turn_id: turn_id.clone(),
                state: TimelineTurnState::Completed,
                failure: None,
            },
        ) {
            log::warn!("[timeline-rebuild] seal turn {turn_id} failed: {error}");
            return None;
        }
    }

    let snapshot = appender.snapshot(seed)?;
    let journal = appender.replay_since(seed, 0);
    Some((snapshot, journal))
}

fn rebuild_text_block(
    appender: &mut crate::timeline::TimelineAppender,
    seed: &str,
    turn_id: &str,
    round_num: u32,
    block_id: &str,
    kind: TimelineBlockKind,
    text: &str,
) -> Option<()> {
    if let Err(error) = appender.apply_intent(
        seed,
        TimelineIntent::BlockOpened {
            turn_id: turn_id.to_string(),
            round_num,
            block_id: block_id.to_string(),
            kind,
            tool: None,
        },
    ) {
        log::warn!("[timeline-rebuild] open {kind:?} block failed: {error}");
        return None;
    }
    if let Err(error) = appender.apply_intent(
        seed,
        TimelineIntent::BlockCheckpoint {
            turn_id: turn_id.to_string(),
            round_num,
            block_id: block_id.to_string(),
            text: text.to_string(),
        },
    ) {
        log::warn!("[timeline-rebuild] checkpoint {kind:?} block failed: {error}");
        return None;
    }
    if let Err(error) = appender.apply_intent(
        seed,
        TimelineIntent::BlockSealed {
            turn_id: turn_id.to_string(),
            round_num,
            block_id: block_id.to_string(),
        },
    ) {
        log::warn!("[timeline-rebuild] seal {kind:?} block failed: {error}");
        return None;
    }
    Some(())
}

fn rebuild_tool(
    card: &qaqh_proto::ToolCallDef,
    results: &[qaqh_proto::ToolResultDef],
) -> TimelineTool {
    let result = results.iter().find(|result| result.tool_call_id == card.id);
    let success = result.is_some_and(|result| result.success);
    let state = if success {
        TimelineToolState::Succeeded
    } else {
        TimelineToolState::Failed
    };
    let failure = (!success).then(|| TimelineFailure {
        code: "tool_execution_failed".into(),
        message: result
            .map(|result| result.output.clone())
            .filter(|output| !output.is_empty())
            .unwrap_or_else(|| "tool result missing in archived messages".into()),
    });
    let summary = result.map(|result| {
        result
            .output
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(120)
            .collect()
    });
    TimelineTool {
        tool_call_id: card.id.clone(),
        name: card.name.clone(),
        state,
        summary,
        args_json: Some(card.args_json.clone()),
        output: result.map(|result| result.output.clone()),
        diff: None,
        progress: String::new(),
        failure,
        permission: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_proto::{RoundBlock, RoundData, ToolCallDef, ToolResultDef, TurnData};

    fn turn_with_blocks() -> TurnData {
        TurnData {
            turn_id: "t1".into(),
            user_text: "question".into(),
            rounds: vec![RoundData {
                round_num: 0,
                is_final: true,
                thinking: None,
                answer: None,
                tool_calls: vec![ToolCallDef {
                    id: "call-1".into(),
                    name: "read".into(),
                    args_display: "read".into(),
                    args_json: "{\"path\":\"/tmp/a\"}".into(),
                }],
                tool_results: vec![ToolResultDef {
                    tool_call_id: "call-1".into(),
                    output: "line one\nline two".into(),
                    success: true,
                    file: None,
                }],
                blocks: vec![
                    RoundBlock::Reasoning {
                        content: "thinking".into(),
                    },
                    RoundBlock::Tool {
                        card: ToolCallDef {
                            id: "call-1".into(),
                            name: "read".into(),
                            args_display: "read".into(),
                            args_json: "{\"path\":\"/tmp/a\"}".into(),
                        },
                    },
                    RoundBlock::Text {
                        content: "answer".into(),
                    },
                ],
            }],
        }
    }

    #[test]
    fn rebuild_preserves_round_blocks_and_tool_terminal_state() {
        let (snapshot, journal) =
            timeline_snapshot_from_turns("seed", &[turn_with_blocks()]).expect("snapshot rebuilt");
        assert_eq!(snapshot.turns.len(), 1);
        assert!(snapshot.watermark > 0);
        assert_eq!(journal.len() as u64, snapshot.watermark);
        assert_eq!(journal.first().expect("opened").timeline_seq, 1);
        assert_eq!(
            journal.last().expect("sealed").timeline_seq,
            snapshot.watermark
        );

        let rebuilt = &snapshot.turns[0];
        assert_eq!(rebuilt.turn_id, "t1");
        assert_eq!(rebuilt.user_text, "question");
        assert!(rebuilt.sealed);
        assert_eq!(rebuilt.state, qaqh_domain::TimelineTurnState::Completed);

        let round = &rebuilt.rounds[0];
        assert!(round.sealed);
        assert!(round.is_final);
        assert_eq!(round.blocks.len(), 3);
        assert_eq!(
            round.blocks[0].kind,
            qaqh_domain::TimelineBlockKind::Reasoning
        );
        assert_eq!(round.blocks[0].text, "thinking");
        assert_eq!(
            round.blocks[0].state,
            qaqh_domain::TimelineBlockState::Sealed
        );

        assert_eq!(round.blocks[1].kind, qaqh_domain::TimelineBlockKind::Tool);
        let tool = round.blocks[1].tool.as_ref().expect("tool block");
        assert_eq!(tool.tool_call_id, "call-1");
        assert_eq!(tool.name, "read");
        assert_eq!(tool.state, qaqh_domain::TimelineToolState::Succeeded);
        assert_eq!(tool.output.as_deref(), Some("line one\nline two"));
        assert_eq!(tool.summary.as_deref(), Some("line one"));
        assert!(tool.args_json.is_some());
        assert!(tool.failure.is_none());

        assert_eq!(round.blocks[2].kind, qaqh_domain::TimelineBlockKind::Text);
        assert_eq!(round.blocks[2].text, "answer");
    }

    #[test]
    fn rebuild_marks_failed_tool_as_failed() {
        let mut turn = turn_with_blocks();
        turn.rounds[0].blocks = vec![RoundBlock::Tool {
            card: ToolCallDef {
                id: "call-1".into(),
                name: "bash".into(),
                args_display: "bash".into(),
                args_json: "{}".into(),
            },
        }];
        turn.rounds[0].tool_calls = vec![ToolCallDef {
            id: "call-1".into(),
            name: "bash".into(),
            args_display: "bash".into(),
            args_json: "{}".into(),
        }];
        turn.rounds[0].tool_results = vec![ToolResultDef {
            tool_call_id: "call-1".into(),
            output: "boom".into(),
            success: false,
            file: None,
        }];

        let (snapshot, _) =
            timeline_snapshot_from_turns("seed", &[turn]).expect("snapshot rebuilt");
        let tool = snapshot.turns[0].rounds[0].blocks[0]
            .tool
            .as_ref()
            .expect("tool block");
        assert_eq!(tool.state, qaqh_domain::TimelineToolState::Failed);
        assert_eq!(tool.failure.as_ref().expect("failure").message, "boom");
    }

    #[test]
    fn rebuild_falls_back_to_legacy_thinking_and_answer_fields() {
        let turn = TurnData {
            turn_id: "t1".into(),
            user_text: "question".into(),
            rounds: vec![RoundData {
                round_num: 0,
                is_final: true,
                thinking: Some("thinking".into()),
                answer: Some("answer".into()),
                tool_calls: vec![],
                tool_results: vec![],
                blocks: vec![],
            }],
        };
        let (snapshot, _) =
            timeline_snapshot_from_turns("seed", &[turn]).expect("snapshot rebuilt");
        let blocks = &snapshot.turns[0].rounds[0].blocks;
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].text, "thinking");
        assert_eq!(blocks[1].text, "answer");
    }

    #[test]
    fn rebuild_returns_none_for_empty_history() {
        assert!(timeline_snapshot_from_turns("seed", &[]).is_none());
    }
}
