//! 折叠策略的作用域契约：一个会话切换工具模式，不得改变其它会话的折叠行为。
//!
//! # 背景
//!
//! `tool_side_fold` 的当前策略原先是**进程级** `static`，而它的唯一写方
//! `AgentState::apply_tool_mode`（`qaqh-runtime/src/agent/state/agent.rs`）是
//! **按会话**执行的——每个会话一个 actor 线程。于是会话 A 切到 minimal 系列
//! （联动 `NoFoldPolicy`）时，同进程内会话 B 的 `exec` 输出也会跟着不再截断。
//!
//! 对比之下，`qaqh-workspace::runtime` 的模块文档明载 per-actor 状态应放在
//! thread-local：`RUNTIME_CTX` / `ACTOR_TOOL_MANAGER` / `AGENT_MODE` / sandbox
//! 都已迁入，折叠策略是漏网的那个。修复后策略随线程（进而随 `ActorToolScope`）
//! 走。
//!
//! # 为何独立成测试二进制
//!
//! 策略读写是线程级状态，与 `tool_side_fold` 的内联单元测试（它们假定全程
//! `StandardPolicy`）同进程并行会互相干扰，因此单独放一个文件。
//!
//! # 判据
//!
//! 1. 另一会话切到 minimal 前后，本会话对同一份 `exec` 输出的折叠结果必须
//!    逐字符一致（策略泄漏时结果会从 8K 量级跳到 24K 硬顶）。
//! 2. `ActorToolScope` 必须把策略带到工具工作线程；未安装作用域的线程保持
//!    标准策略。

use std::sync::Arc;
use std::sync::mpsc;

use qaqh_workspace::runtime::ActorToolScope;
use qaqh_workspace::tool_side_fold::{self, NoFoldPolicy, StandardPolicy};

/// 标准策略下 `exec` 的字符上限是 8K；`ToolResult::ok` 的模型面硬顶是 24K。
/// 观测值若超过这个界，说明标准折叠没生效（即策略被换成了 NoFold）。
const STANDARD_FOLD_UPPER_BOUND: usize = 12_000;

/// 在当前线程（视作某个 actor 线程）上跑一次 `exec` 结果折叠，返回模型可见
/// 文本的字符数。这是策略的可观测量：8K 限额 vs 24K 硬顶，差异足够大。
fn folded_exec_chars() -> usize {
    let body = "line of output\n".repeat(2_000); // ~30K chars，远超 8K 命令输出限额
    let mut result = qaqh_types::ToolResult::ok(body);
    tool_side_fold::apply("exec", &mut result);
    result.model.text.chars().count()
}

#[test]
fn another_session_switching_to_minimal_must_not_change_this_sessions_folding() {
    tool_side_fold::set_thread_policy(Arc::new(StandardPolicy));

    let (baseline_tx, baseline_rx) = mpsc::channel::<usize>();
    let (switched_tx, switched_rx) = mpsc::channel::<()>();

    let (baseline, folded_after_switch) = std::thread::scope(|scope| {
        // 「本会话」：全程保持标准模式，不主动切换任何策略。
        let this_session = scope.spawn(move || {
            let baseline = folded_exec_chars();
            baseline_tx.send(baseline).expect("baseline send");
            switched_rx.recv().expect("switched recv");
            (baseline, folded_exec_chars())
        });

        let baseline = baseline_rx.recv().expect("baseline recv");
        assert!(
            baseline < STANDARD_FOLD_UPPER_BOUND,
            "前提失败：标准策略下 exec 输出应被折叠到 8K 量级，实际 {baseline} 字符"
        );

        // 「另一会话」切到 minimal 系列，联动 NoFoldPolicy。
        tool_side_fold::set_thread_policy(Arc::new(NoFoldPolicy));
        switched_tx.send(()).expect("switched send");

        this_session.join().expect("join this session")
    });

    // 还原本线程策略，避免影响本二进制内的其它测试。
    tool_side_fold::set_thread_policy(Arc::new(StandardPolicy));

    assert_eq!(
        folded_after_switch, baseline,
        "折叠策略跨会话泄漏：另一会话切到 minimal 后，本会话的 exec 输出从 {baseline} \
         字符变成 {folded_after_switch} 字符（策略应随 actor 线程，而非进程级 static）"
    );
}

#[test]
fn actor_tool_scope_carries_the_fold_policy_to_worker_threads() {
    // actor 线程（本线程）切到极限模式，然后捕获作用域交给工具工作线程。
    tool_side_fold::set_thread_policy(Arc::new(NoFoldPolicy));
    let scope = ActorToolScope::capture();

    let inherited = std::thread::scope(|s| {
        s.spawn(|| {
            let _guard = scope.install();
            folded_exec_chars()
        })
        .join()
        .expect("join worker")
    });

    // 未安装 actor 作用域的普通线程：应保持标准策略。
    let untouched = std::thread::scope(|s| {
        s.spawn(folded_exec_chars)
            .join()
            .expect("join plain thread")
    });

    tool_side_fold::set_thread_policy(Arc::new(StandardPolicy));

    assert!(
        untouched < STANDARD_FOLD_UPPER_BOUND,
        "未安装 actor 作用域的线程应保持标准折叠，实际 {untouched} 字符"
    );
    assert!(
        inherited > untouched,
        "ActorToolScope 未把 actor 的 NoFoldPolicy 带到工具工作线程：\
         继承到的结果 {inherited} 字符，未能长于标准折叠的 {untouched} 字符"
    );
}
