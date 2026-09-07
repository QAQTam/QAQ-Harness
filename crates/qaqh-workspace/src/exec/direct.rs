//! exec::direct — direct_exec 主引擎 + ExecOutput（registry-native 生命周期）。

use std::sync::{Arc, atomic::AtomicU64};

use serde::Serialize;

use crate::{ExecOutputStream, ExecProgressSender};

#[cfg(windows)]
use super::pipe::pipe_available_bytes;
#[cfg(unix)]
use super::pipe::set_pipe_nonblocking;
use super::pipe::{PipePumpCtx, Readiness, SEAL_JOIN_BUDGET, spawn_pipe_reader, wait_reader_done};
use super::truncate::{strip_ansi, token_truncate};

/// Direct command execution: argv array, no shell.
/// Uses background threads for pipe reading and poll-based timeout.
#[allow(clippy::too_many_arguments)] // 参数面塑形另立项（PLAN D-5）
pub(crate) fn direct_exec(
    argv: &[String],
    env: Option<&[(String, String)]>,
    cwd: Option<&str>,
    max_output_tokens: u32,
    timeout_secs: u64,
    background_after_secs: Option<u64>,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    progress_tx: Option<ExecProgressSender>,
    tool_call_id: &str,
) -> ExecOutput {
    let start_time = std::time::Instant::now();
    let display_name = if argv.len() > 1 {
        format!("{} ...", argv[0])
    } else {
        argv[0].clone()
    };
    let mut cmd = std::process::Command::new(&argv[0]);
    if argv.len() > 1 {
        cmd.args(&argv[1..]);
    }
    if let Some(env) = env {
        cmd.envs(env.iter().map(|(k, v)| (k, v)));
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        // H6：独立进程组——取消/超时 kill 时可对整组 SIGKILL，
        // 孙进程不再持有管道写端导致 reader 永不 EOF。
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return ExecOutput {
                status: "completed",
                command: display_name,
                exit_code: Some(-1),
                output: format!("SPAWN FAILED: {e}"),
                truncated: false,
                timed_out: false,
                cancelled: false,
                process_id: None,
            };
        }
    };

    // 接线 ProcessRegistry：先注册（读线程捕获 proc_id），take 管道后
    // 再把子进程句柄移入注册表（poll 经 try_wait、超时移交可查）。
    let proc_id = crate::process_registry::ProcessRegistry::register(&display_name);

    // Start bounded pipe readers（registry-native，阶段 2）：
    // - 读线程有界退出（EOF / settle 到期 / 读错误，任一原因发退出信号）；
    // - 输出完整捕获进注册表（tail 视图 + captured_full 权威源）；
    // - progress sender 随线程结束必然 drop（drain 的 Disconnected 快路径）。
    // 旧"汇总信道 + handoff 善后轮询"退役：try_wait 已在任何查询路径
    // 自动置终态，善后块是死代码；汇总数据源被 captured_full 取代。
    let (stdout_done_tx, stdout_done_rx) = std::sync::mpsc::channel::<(bool, bool)>();
    let (stderr_done_tx, stderr_done_rx) = std::sync::mpsc::channel::<(bool, bool)>();
    let progress_seq = Arc::new(AtomicU64::new(0));
    let byte_cap = crate::process_registry::FULL_CAPTURE_BYTE_CAP;
    let stdout_ctx = PipePumpCtx {
        progress_tx: progress_tx.clone(),
        tool_call_id: tool_call_id.to_string(),
        output_stream: ExecOutputStream::Stdout,
        progress_seq: progress_seq.clone(),
        registry_id: proc_id,
    };
    let stderr_ctx = PipePumpCtx {
        progress_tx: progress_tx.clone(),
        tool_call_id: tool_call_id.to_string(),
        output_stream: ExecOutputStream::Stderr,
        progress_seq: progress_seq.clone(),
        registry_id: proc_id,
    };
    if let Some(p) = child.stdout.take() {
        #[cfg(unix)]
        set_pipe_nonblocking(&p);
        #[cfg(unix)]
        spawn_pipe_reader(
            p,
            byte_cap,
            stdout_ctx,
            |_stream: &mut std::process::ChildStdout| Ok(Readiness::Ready),
            stdout_done_tx,
        );
        #[cfg(windows)]
        spawn_pipe_reader(
            p,
            byte_cap,
            stdout_ctx,
            |stream: &mut std::process::ChildStdout| {
                use std::os::windows::io::AsRawHandle;
                Ok(match pipe_available_bytes(stream.as_raw_handle()) {
                    Some(0) => Readiness::Empty,
                    Some(_) => Readiness::Ready,
                    None => Readiness::Closed,
                })
            },
            stdout_done_tx,
        );
    } else {
        let _ = stdout_done_tx.send((true, false));
    }
    if let Some(p) = child.stderr.take() {
        #[cfg(unix)]
        set_pipe_nonblocking(&p);
        #[cfg(unix)]
        spawn_pipe_reader(
            p,
            byte_cap,
            stderr_ctx,
            |_stream: &mut std::process::ChildStderr| Ok(Readiness::Ready),
            stderr_done_tx,
        );
        #[cfg(windows)]
        spawn_pipe_reader(
            p,
            byte_cap,
            stderr_ctx,
            |stream: &mut std::process::ChildStderr| {
                use std::os::windows::io::AsRawHandle;
                Ok(match pipe_available_bytes(stream.as_raw_handle()) {
                    Some(0) => Readiness::Empty,
                    Some(_) => Readiness::Ready,
                    None => Readiness::Closed,
                })
            },
            stderr_done_tx,
        );
    } else {
        let _ = stderr_done_tx.send((true, false));
    }
    crate::process_registry::ProcessRegistry::attach_child(proc_id, child);

    // Poll child with timeout（子进程句柄唯一持有在注册表，经 try_wait 查询）
    let deadline = start_time + std::time::Duration::from_secs(timeout_secs);
    // 快速移交：子进程存活超过 background_after_secs 即移交后台（不等 timeout）。
    // 用于拉起长驻服务（serve/daemon/watch）—���调用方希望尽快拿到
    // backgrounded tool_result，用 process(action=check/wait/kill) 接管，而不是
    // 死等到 timeout_secs 让 agent loop 阻塞。
    let handoff_deadline =
        background_after_secs.map(|secs| start_time + std::time::Duration::from_secs(secs));
    let mut exit_code: Option<i32> = None;
    let mut timed_out = false;
    let mut cancelled = false;
    loop {
        match crate::process_registry::ProcessRegistry::try_wait(proc_id) {
            Some(code) => {
                exit_code = Some(code);
                break;
            }
            None => {
                if cancel.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::SeqCst))
                    || crate::is_cancel()
                {
                    // 取消 = 杀进程树（含后代），��止管道泄漏
                    crate::process_registry::ProcessRegistry::kill(proc_id);
                    cancelled = true;
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    // 超时 = 移交后台（不 kill）：进程存活、管道线程继续
                    // append_output/推流，LLM 可用 process(action=...) 接管。
                    timed_out = true;
                    break;
                }
                if handoff_deadline.is_some_and(|hd| std::time::Instant::now() >= hd) {
                    // 快速移交：进程仍在运行且已超过观察窗口 → 立即转后台。
                    timed_out = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }

    // 超时移交：不再等待管道（读取线程仍在后台 append 到注册表）
    if timed_out {
        let info = crate::process_registry::ProcessRegistry::get_info(proc_id)
            .unwrap_or_else(|| serde_json::json!({}));
        return ExecOutput {
            status: "backgrounded",
            command: display_name,
            exit_code: None,
            output: serde_json::json!({
                "backgrounded": true,
                "process_id": proc_id,
                "transferred_after_secs": start_time.elapsed().as_secs_f64(),
                "hint": "进程已转入后台（未终止）。用 process(action=\"check\", id=process_id) 查看状态，process(action=\"wait\", id=process_id) 等待完成，process(action=\"kill\", id=process_id) 终止。",
                "info": info,
            })
            .to_string(),
            truncated: false,
            timed_out: true,
            cancelled: false,
            process_id: Some(proc_id),
        };
    }

    // 正常退出 / 取消：标记注册表状态
    if cancelled {
        crate::process_registry::ProcessRegistry::kill(proc_id);
    } else if let Some(code) = exit_code {
        crate::process_registry::ProcessRegistry::mark_exited(proc_id, code);
    }

    // Collect pipe output（P0 去 EOF 化，2026-09-02 冻结事故）：
    // [seal] registry-native（exec 生命周期重写阶段 2，2.1 契约）：
    // 以注册表完整捕获 `captured_full` 为权威——任何路径不得等待管道
    // EOF / 流关闭。对读线程做的是"有界 join"（SEAL_JOIN_BUDGET）：退出
    // 信号在 EOF、settle 到期、读错误时都会发出；正常路径读线程先于
    // seal 完成，首次 recv 立即返回、零额外等待；孙进程持写端路径信号
    // 来自 settle 到期（读线程确定性退出），快照始终完整可用。
    let join_deadline = std::time::Instant::now() + SEAL_JOIN_BUDGET;
    let (stdout_eof, stdout_capped) = wait_reader_done(&stdout_done_rx, join_deadline);
    let (stderr_eof, stderr_capped) = wait_reader_done(&stderr_done_rx, join_deadline);
    let (stdout_out, stderr_out) = crate::process_registry::ProcessRegistry::captured_full(proc_id)
        .unwrap_or_else(|| {
            // 防御性：seal 紧跟退出执行，条目惰性驱逐（10 分钟终态门槛）
            // 不可能触发；缺失意味着未知的并发破坏——占位并按截断处理。
            log::warn!("[exec] registry full capture missing for process {proc_id}");
            (
                "[WARN] registry full capture missing\n".to_string(),
                String::new(),
            )
        });
    let mut combined = String::new();
    if !stderr_out.is_empty() {
        combined.push_str(&stderr_out);
        if !stdout_out.is_empty() {
            combined.push('\n');
        }
    }
    combined.push_str(&stdout_out);
    // truncated 口径：字节预算耗尽（任一流）或读线程未以 EOF 收尾
    // （settle 放弃 = 孙进程可能继续产出，保守提示输出可能不完整）。
    let hard_trunc = !stdout_eof || !stderr_eof || stdout_capped || stderr_capped;
    let cleaned = strip_ansi(&combined);
    let total_tokens = qaqh_types::token::count_tokens(&cleaned);
    let (output_str, truncated) = if total_tokens > max_output_tokens || hard_trunc {
        (token_truncate(&cleaned, max_output_tokens), true)
    } else {
        (cleaned, false)
    };

    ExecOutput {
        status: if cancelled { "cancelled" } else { "completed" },
        command: display_name,
        exit_code,
        output: output_str,
        truncated,
        timed_out,
        cancelled,
        process_id: Some(proc_id),
    }
}

/// Structured output from a command execution.
#[derive(Serialize, Debug, Clone)]
pub(crate) struct ExecOutput {
    pub(crate) status: &'static str,
    pub(crate) command: String,
    pub(crate) exit_code: Option<i32>,
    pub(crate) output: String,
    pub(crate) truncated: bool,
    pub(crate) timed_out: bool,
    pub(crate) cancelled: bool,
    /// 超时移交后台时的注册表进程 id（由 process 的 action 使用）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) process_id: Option<u32>,
}

impl ExecOutput {
    pub(crate) fn to_json(&self) -> String {
        serde_json::to_string(self)
            .unwrap_or_else(|_| r#"{"status":"error","output":"serialization failed"}"#.into())
    }
}
