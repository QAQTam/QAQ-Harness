//! End-to-end BUG-006 regression: a missing timeline file must be rebuilt
//! from `messages.jsonl`, so deleting the projection never deletes history.

use std::path::PathBuf;

use qaqh_domain::{TimelineBlockKind, TimelineToolState, TimelineTurnState};
use qaqh_runtime::RingingHub;
use qaqh_session::SessionManager;
use qaqh_types::{ContentBlock, Message};

fn temp_root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "qaqh-timeline-rebuild-{}-{}",
        std::process::id(),
        tag
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create temp root");
    root
}

fn session_messages() -> Vec<Message> {
    let assistant = Message {
        msg_id: None,
        role: Message::ROLE_ASSISTANT.to_string(),
        name: None,
        content: vec![
            ContentBlock::Reasoning {
                reasoning: "thinking".to_string(),
            },
            ContentBlock::ToolUse {
                id: "call-1".to_string(),
                name: "read".to_string(),
                input: serde_json::json!({ "path": "/tmp/a" }),
            },
            ContentBlock::Text {
                text: "answer".to_string(),
            },
        ],
    };
    vec![
        Message::system("system"),
        Message::user("question"),
        assistant,
        Message::tool("call-1", "line one\nline two", true),
    ]
}

#[test]
fn missing_or_corrupt_timeline_is_rebuilt_from_persisted_messages() {
    let root = temp_root("e2e");
    SessionManager::init(root.clone());
    let seed = "rebuilt-seed";
    SessionManager::global().save_append(seed, &session_messages(), "test-model", None, 0, 1);

    // 第二个会话模拟“timeline 文件存在但已损坏”：文件名仍能进懒加载索引，
    // 内容无法解析，恢复路径必须退回 messages.jsonl 重建而不是返回空快照。
    let corrupt_seed = "corrupt-timeline-seed";
    SessionManager::global().save_append(
        corrupt_seed,
        &session_messages(),
        "test-model",
        None,
        0,
        1,
    );
    let ringing_root = root.join("ringing");
    let corrupt_timeline = ringing_root
        .join("ringing-timeline")
        .join(format!("{corrupt_seed}.json"));
    std::fs::create_dir_all(corrupt_timeline.parent().expect("timeline dir"))
        .expect("create timeline dir");
    std::fs::write(&corrupt_timeline, b"{not-json").expect("write corrupt timeline");

    // `ringing/` 的其它 seed 没有任何 timeline 记录，
    // 相当于用户删除了整个 timeline 目录。
    let hub = RingingHub::with_persistence("epoch-rebuild", ringing_root);
    let snapshot = hub
        .timeline_snapshot(seed)
        .expect("timeline must be rebuilt from messages.jsonl");

    assert_eq!(snapshot.turns.len(), 1);
    let turn = &snapshot.turns[0];
    assert_eq!(turn.user_text, "question");
    assert!(turn.sealed);
    assert_eq!(turn.state, TimelineTurnState::Completed);
    assert_eq!(turn.rounds.len(), 1);
    let round = &turn.rounds[0];
    assert!(round.sealed);
    assert!(round.is_final);
    assert_eq!(round.blocks.len(), 3);
    assert_eq!(round.blocks[0].kind, TimelineBlockKind::Reasoning);
    assert_eq!(round.blocks[0].text, "thinking");
    assert_eq!(
        round.blocks[1].tool.as_ref().expect("tool block").state,
        TimelineToolState::Succeeded
    );
    assert_eq!(round.blocks[2].text, "answer");

    // 第二次读取走的是刚写回的 timeline 文件，结果必须一致。
    let second = hub
        .timeline_snapshot(seed)
        .expect("persisted rebuilt timeline");
    assert_eq!(second, snapshot);

    // 损坏记录也必须从 messages.jsonl 重建。
    let corrupted = hub
        .timeline_snapshot(corrupt_seed)
        .expect("corrupt timeline rebuilt from messages.jsonl");
    assert_eq!(corrupted.turns.len(), 1);
    assert_eq!(corrupted.turns[0].user_text, "question");
    assert_eq!(
        std::fs::read(&corrupt_timeline)
            .expect("rebuilt file readable")
            .len()
            > 0,
        true
    );

    drop(hub);
    let _ = std::fs::remove_dir_all(root);
}
